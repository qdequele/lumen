//! Platform provider key pools (ADR 019): the platform-only admin API under
//! `/admin/platform-keys`.
//!
//! The platform's own keys (Meilisearch's, on the hosted gateway) per
//! provider, rotated on the request path like team keys (ADR 018) wherever
//! the platform key serves. A provider with at least one platform key,
//! enabled or not, ignores its configured key (`api_key_env` /
//! `provider_keys`); with none, the configured key serves as before. These
//! routes sit with the other platform routes:
//! master key only, and a call scoped with `X-Lumen-Account-Ref` is refused
//! `403 LM-4005`, so no team reaches them through a control plane.
//!
//! Keys are sealed with the master key at rest, never returned (only `label`
//! and `last4`) and never logged. Every write pings the reload trigger; a
//! disable or delete also evicts the key from the live pool before
//! answering.

use crate::admin::{internal, json_body, runtime};
use crate::byok_admin::{
    document_providers, ineligible, request_reload, unknown_provider, MAX_KEY_BYTES,
    MAX_LABEL_CHARS, MIN_KEY_CHARS,
};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::store::PlatformProviderKeyRecord;
use lumen_auth::AuthError;
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};

/// `POST .../keys` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddPlatformKeyBody {
    /// The provider API key to seal. Never logged, never returned.
    pub key: String,
    /// A human label, 1 to 64 characters.
    pub label: String,
}

// Hand-written so a stray `{:?}` can never print the key.
impl std::fmt::Debug for AddPlatformKeyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddPlatformKeyBody")
            .field("key", &"<redacted>")
            .field("label", &self.label)
            .finish()
    }
}

/// `PATCH .../keys/{key_id}` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchPlatformKeyBody {
    /// Whether the key is in the live pool.
    pub enabled: bool,
}

/// One platform key as the API shows it: never the secret.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformKeyView {
    /// `ppk_` + random hex.
    pub id: String,
    /// The human label.
    pub label: String,
    /// Last 4 characters of the key.
    pub last4: String,
    /// Whether the key is in the live pool.
    pub enabled: bool,
    /// `unused` | `ok` | `rate_limited` | `failing`: the key's live health
    /// on the request path (`unused` until a request uses it, and while
    /// disabled).
    pub status: &'static str,
    /// Why the key is not `ok`, when known.
    pub status_detail: Option<String>,
    /// Creation time, unix seconds.
    pub created_at: i64,
}

impl PlatformKeyView {
    fn new(state: &AppState, record: PlatformProviderKeyRecord) -> Self {
        let (status, status_detail) = if record.enabled {
            state.team_pools.platform_status(&record.id)
        } else {
            ("unused", None)
        };
        Self {
            id: record.id,
            label: record.label,
            last4: record.last4,
            enabled: record.enabled,
            status,
            status_detail,
            created_at: record.created_at,
        }
    }
}

/// One configured provider with its platform pool.
#[derive(Debug, Serialize)]
pub struct PlatformProvider {
    /// The configured provider instance name.
    pub name: String,
    /// Its kind (`openai`, `anthropic`, ...).
    pub kind: &'static str,
    /// Whether the kind takes a pool key (plain API key kinds only).
    pub eligible: bool,
    /// Whether the live provider also has a configured key (its
    /// `api_key_env` variable or a `PUT /admin/provider-keys` row). It
    /// serves only while `keys` is empty: once the provider has a platform
    /// key, enabled or not, the configured key is ignored.
    pub configured_key: bool,
    /// The model ids the provider serves.
    pub models: Vec<String>,
    /// The pool's keys, oldest first.
    pub keys: Vec<PlatformKeyView>,
}

/// `GET /admin/platform-keys` response.
#[derive(Debug, Serialize)]
pub struct PlatformProviders {
    /// Every provider of the current config document.
    pub providers: Vec<PlatformProvider>,
}

fn unknown_key(key_id: &str) -> ApiError {
    GatewayError::NotFound(format!("unknown platform key id '{key_id}'")).into()
}

/// List every provider of the config with its platform pool.
///
/// # Errors
/// `LM-5001` (500) on a store or config source error.
pub async fn list_platform_keys(
    State(state): State<AppState>,
) -> Result<Json<PlatformProviders>, ApiError> {
    let auth = runtime(&state)?;
    let configured = document_providers(&state).await?;
    let mut keys = auth
        .store
        .list_platform_provider_keys()
        .await
        .map_err(|e| internal(&e))?;
    let providers = configured
        .into_iter()
        .map(|p| {
            let (mine, rest): (Vec<_>, Vec<_>) = keys.drain(..).partition(|k| k.provider == p.name);
            keys = rest;
            PlatformProvider {
                kind: p.kind.as_str(),
                eligible: p.kind.byok_eligible(),
                configured_key: state.registry.has_api_key(&p.name),
                models: p.models.iter().map(|m| m.id.clone()).collect(),
                keys: mine
                    .into_iter()
                    .map(|record| PlatformKeyView::new(&state, record))
                    .collect(),
                name: p.name,
            }
        })
        .collect();
    Ok(Json(PlatformProviders { providers }))
}

/// Seal and store a platform key, enabled.
///
/// # Errors
/// `LM-1003` (404) for an unknown provider; `LM-1001` (400) for an
/// ineligible kind, a key shorter than 16 characters, a bad label or a full
/// pool.
pub async fn add_platform_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    payload: Result<Json<AddPlatformKeyBody>, JsonRejection>,
) -> Result<(StatusCode, Json<PlatformKeyView>), ApiError> {
    let auth = runtime(&state)?;
    // JsonRejection text describes the shape, never echoes field values.
    let body = json_body(&state, payload)?;
    let key = body.key.trim();
    if key.chars().count() < MIN_KEY_CHARS {
        return Err(GatewayError::InvalidRequest(format!(
            "`key` must be at least {MIN_KEY_CHARS} characters"
        ))
        .into());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(GatewayError::InvalidRequest(format!(
            "`key` must be at most {MAX_KEY_BYTES} bytes"
        ))
        .into());
    }
    let label = body.label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(GatewayError::InvalidRequest(format!(
            "`label` must be 1 to {MAX_LABEL_CHARS} characters"
        ))
        .into());
    }
    let Some(master) = auth.master.as_ref() else {
        return Err(GatewayError::Internal("master key unavailable".to_owned()).into());
    };
    // Under the config lock so a provider delete cannot slip between the
    // check and the insert and leave an orphaned pool behind.
    let _guard = state.config_apply_lock.lock().await;
    let configured = document_providers(&state).await?;
    let found = configured
        .iter()
        .find(|p| p.name == provider)
        .ok_or_else(|| unknown_provider(&provider))?;
    if !found.kind.byok_eligible() {
        return Err(ineligible(found));
    }
    let record = auth
        .store
        .add_platform_provider_key(&provider, label, key, master)
        .await
        .map_err(|error| match error {
            AuthError::PoolFull(max) => GatewayError::InvalidRequest(format!(
                "at most {max} platform keys per provider; delete one first"
            ))
            .into(),
            other => internal(&other),
        })?;
    request_reload(&state);
    tracing::info!(%provider, key_id = %record.id, "platform provider key added");
    Ok((
        StatusCode::CREATED,
        Json(PlatformKeyView::new(&state, record)),
    ))
}

/// Turn one platform key on or off.
///
/// # Errors
/// `LM-1003` (404) for an unknown provider or key.
pub async fn patch_platform_key(
    State(state): State<AppState>,
    Path((provider, key_id)): Path<(String, String)>,
    payload: Result<Json<PatchPlatformKeyBody>, JsonRejection>,
) -> Result<Json<PlatformKeyView>, ApiError> {
    let auth = runtime(&state)?;
    let body = json_body(&state, payload)?;
    let record = auth
        .store
        .set_platform_provider_key_enabled(&provider, &key_id, body.enabled)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| unknown_key(&key_id))?;
    if !record.enabled {
        // Out of the live pool before answering; the reload reconciles. The
        // row stays, so the pool does too: the configured key stays ignored.
        state.team_pools.remove_platform(&provider, &key_id, false);
    }
    request_reload(&state);
    tracing::info!(%provider, %key_id, enabled = record.enabled, "platform provider key toggled");
    Ok(Json(PlatformKeyView::new(&state, record)))
}

/// Delete one platform key.
///
/// # Errors
/// `LM-1003` (404) for an unknown provider or key.
pub async fn delete_platform_key(
    State(state): State<AppState>,
    Path((provider, key_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    let deleted = auth
        .store
        .delete_platform_provider_key(&provider, &key_id)
        .await
        .map_err(|e| internal(&e))?;
    if !deleted {
        return Err(unknown_key(&key_id));
    }
    // The provider's last platform key: its pool goes, and the configured
    // key serves again.
    let last = !auth
        .store
        .list_platform_provider_keys()
        .await
        .map_err(|e| internal(&e))?
        .iter()
        .any(|k| k.provider == provider);
    state.team_pools.remove_platform(&provider, &key_id, last);
    request_reload(&state);
    tracing::info!(%provider, %key_id, "platform provider key deleted");
    Ok(StatusCode::NO_CONTENT)
}
