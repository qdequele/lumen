//! Team provider key pools (BYOK): the account-scoped admin API under
//! `/admin/groups/{id}/byok`.
//!
//! On the hosted gateway one Lab account is one budget group. A team admin
//! manages that group's key pools per provider through the Lab console; the
//! Lab proxy rewrites `/admin/groups/current/...` to the group id and sends
//! `X-Lumen-Account-Ref`, so every route here is guarded like the other group
//! routes (an out-of-scope id answers the same 404 `LM-1003` as an unknown
//! one).
//!
//! Keys are sealed with the master key at rest, never returned (only `label`
//! and `last4`) and never logged. Every write pings the reload trigger: the
//! request path reads the pools from memory, rebuilt by the reloader.

use crate::admin::{config_ctx, config_from_document, internal, runtime, source_internal_error};
use crate::admin_scope::AccountScope;
use crate::config::ProviderConfig;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::AuthError;
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};

/// Longest accepted label, in characters.
const MAX_LABEL_CHARS: usize = 64;
/// Longest accepted key, in bytes (real provider keys are far shorter).
const MAX_KEY_BYTES: usize = 1024;

/// `POST .../keys` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddTeamKeyBody {
    /// The provider API key to seal. Never logged, never returned.
    pub key: String,
    /// A human label, 1 to 64 characters.
    pub label: String,
}

// Hand-written so a stray `{:?}` can never print the key.
impl std::fmt::Debug for AddTeamKeyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddTeamKeyBody")
            .field("key", &"<redacted>")
            .field("label", &self.label)
            .finish()
    }
}

/// `PUT .../{provider}` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamProviderBody {
    /// Fall back to the platform's keys when every team key is rate-limited
    /// or failing.
    pub fallback: bool,
}

/// One team key as the API shows it: never the secret.
#[derive(Debug, Clone, Serialize)]
pub struct TeamKeyView {
    /// `gpk_` + random hex.
    pub id: String,
    /// The human label.
    pub label: String,
    /// Last 4 characters of the key.
    pub last4: String,
    /// `unused` | `ok` | `rate_limited` | `failing`. Always `unused` until the
    /// request path reports live key health.
    pub status: &'static str,
    /// Why the key is not `ok`, when known.
    pub status_detail: Option<String>,
    /// Creation time, unix seconds.
    pub created_at: i64,
}

impl From<lumen_auth::store::GroupProviderKeyRecord> for TeamKeyView {
    fn from(record: lumen_auth::store::GroupProviderKeyRecord) -> Self {
        Self {
            id: record.id,
            label: record.label,
            last4: record.last4,
            status: "unused",
            status_detail: None,
            created_at: record.created_at,
        }
    }
}

/// One configured provider with the group's pool for it.
#[derive(Debug, Serialize)]
pub struct TeamProvider {
    /// The configured provider instance name.
    pub name: String,
    /// Its kind (`openai`, `anthropic`, ...).
    pub kind: &'static str,
    /// Whether a team may bring its own key for this kind.
    pub eligible: bool,
    /// The model ids the provider serves.
    pub models: Vec<String>,
    /// Whether the platform's keys are the fallback when team keys fail.
    pub fallback: bool,
    /// The group's keys for this provider, oldest first.
    pub keys: Vec<TeamKeyView>,
}

/// `GET /admin/groups/{id}/byok` response.
#[derive(Debug, Serialize)]
pub struct TeamProviders {
    /// Every provider of the current config document.
    pub providers: Vec<TeamProvider>,
}

/// The providers of the persisted config document (not the routing table: a
/// provider added a moment ago is in the document before its hot reload).
async fn document_providers(state: &AppState) -> Result<Vec<ProviderConfig>, ApiError> {
    let ctx = config_ctx(state)?;
    let doc = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    Ok(config_from_document(&doc.toml)?.providers)
}

fn unknown_provider(name: &str) -> ApiError {
    GatewayError::NotFound(format!("unknown provider '{name}'")).into()
}

fn ineligible(provider: &ProviderConfig) -> ApiError {
    GatewayError::InvalidRequest(format!(
        "provider '{}' (kind '{}') does not take a team API key",
        provider.name,
        provider.kind.as_str()
    ))
    .into()
}

/// Ping the reloader so the pools are rebuilt from the store.
fn request_reload(state: &AppState) {
    if let Some(trigger) = &state.reload_trigger {
        trigger.notify_one();
    }
}

/// List every provider of the config with the group's keys and fallback.
///
/// # Errors
/// `LM-1003` (404) for an unknown or out-of-scope group.
pub async fn list_team_providers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    scope: AccountScope,
) -> Result<Json<TeamProviders>, ApiError> {
    let auth = runtime(&state)?;
    scope.require_group(&auth.store, &id).await?;
    let configured = document_providers(&state).await?;
    let mut keys = auth
        .store
        .list_group_provider_keys(&id)
        .await
        .map_err(|e| internal(&e))?;
    let settings = auth
        .store
        .group_provider_settings()
        .await
        .map_err(|e| internal(&e))?;
    let providers = configured
        .into_iter()
        .map(|p| {
            let (mine, rest): (Vec<_>, Vec<_>) = keys.drain(..).partition(|k| k.provider == p.name);
            keys = rest;
            let fallback = settings
                .iter()
                .any(|(group, provider, on)| *on && *group == id && *provider == p.name);
            TeamProvider {
                kind: p.kind.as_str(),
                eligible: p.kind.byok_eligible(),
                models: p.models.iter().map(|m| m.id.clone()).collect(),
                fallback,
                keys: mine.into_iter().map(TeamKeyView::from).collect(),
                name: p.name,
            }
        })
        .collect();
    Ok(Json(TeamProviders { providers }))
}

/// Seal and store a team key.
///
/// # Errors
/// `LM-1003` (404) for an unknown group or provider; `LM-1001` (400) for an
/// ineligible kind, an empty key, a bad label or a full pool.
pub async fn add_team_key(
    State(state): State<AppState>,
    Path((id, provider)): Path<(String, String)>,
    scope: AccountScope,
    payload: Result<Json<AddTeamKeyBody>, JsonRejection>,
) -> Result<(StatusCode, Json<TeamKeyView>), ApiError> {
    let auth = runtime(&state)?;
    scope.require_group(&auth.store, &id).await?;
    // JsonRejection text describes the shape, never echoes field values.
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let key = body.key.trim();
    if key.is_empty() {
        return Err(GatewayError::InvalidRequest("`key` must not be empty".to_owned()).into());
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
        .add_group_provider_key(&id, &provider, label, key, master)
        .await
        .map_err(|error| match error {
            AuthError::PoolFull(max) => GatewayError::InvalidRequest(format!(
                "at most {max} team keys per provider; delete one first"
            ))
            .into(),
            other => internal(&other),
        })?;
    request_reload(&state);
    tracing::info!(group_id = %id, %provider, key_id = %record.id, "team provider key added");
    Ok((StatusCode::CREATED, Json(TeamKeyView::from(record))))
}

/// Delete one team key.
///
/// # Errors
/// `LM-1003` (404) for an unknown group or key.
pub async fn delete_team_key(
    State(state): State<AppState>,
    Path((id, provider, key_id)): Path<(String, String, String)>,
    scope: AccountScope,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    scope.require_group(&auth.store, &id).await?;
    let deleted = auth
        .store
        .delete_group_provider_key(&id, &provider, &key_id)
        .await
        .map_err(|e| internal(&e))?;
    if !deleted {
        return Err(GatewayError::NotFound(format!("unknown team key id '{key_id}'")).into());
    }
    request_reload(&state);
    tracing::info!(group_id = %id, %provider, %key_id, "team provider key deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Turn the platform-key fallback on or off for one pool.
///
/// # Errors
/// `LM-1003` (404) for an unknown group or provider; `LM-1001` (400) for an
/// ineligible kind.
pub async fn put_team_provider(
    State(state): State<AppState>,
    Path((id, provider)): Path<(String, String)>,
    scope: AccountScope,
    payload: Result<Json<TeamProviderBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    scope.require_group(&auth.store, &id).await?;
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let _guard = state.config_apply_lock.lock().await;
    let configured = document_providers(&state).await?;
    let found = configured
        .iter()
        .find(|p| p.name == provider)
        .ok_or_else(|| unknown_provider(&provider))?;
    if !found.kind.byok_eligible() {
        return Err(ineligible(found));
    }
    auth.store
        .set_group_provider_fallback(&id, &provider, body.fallback)
        .await
        .map_err(|e| internal(&e))?;
    request_reload(&state);
    tracing::info!(group_id = %id, %provider, fallback = body.fallback, "team provider fallback set");
    Ok(StatusCode::NO_CONTENT)
}
