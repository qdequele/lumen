//! Minimal admin API (M5 §5.5): key management under `/admin`, protected by
//! the master key (see `auth::require_master_key`).
//!
//! * `POST /admin/keys` — create a key. The response is the ONLY place the
//!   plaintext key ever appears; it is never stored and never logged.
//! * `GET /admin/keys` — list keys (records only, no hashes, no plaintext).
//! * `PATCH /admin/keys/{id}` — adjust budgets/limits, enable/disable.
//! * `PUT /admin/provider-keys/{name}` — store a provider API key encrypted
//!   at rest (AES-256-GCM under the master key); read back at boot for
//!   providers whose `api_key_env` is unset or empty.
//!
//! Every change is applied to the database AND the in-memory state, so it
//! takes effect immediately without a restart.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::key::hash_key;
use lumen_auth::store::{KeyPatch, NewKey, VirtualKeyRecord};
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};

/// `POST /admin/keys` response: the record plus the one-time plaintext key.
#[derive(Debug, Serialize)]
pub struct CreatedKey {
    /// The clear virtual key. Shown exactly once — store it now.
    pub key: String,
    /// The created record.
    #[serde(flatten)]
    pub record: VirtualKeyRecord,
}

/// Map an auth-layer failure to an opaque 500 — never a misleading 401.
fn internal(error: &lumen_auth::AuthError) -> ApiError {
    GatewayError::Internal(error.to_string()).into()
}

fn runtime(state: &AppState) -> Result<&crate::auth::AuthRuntime, ApiError> {
    state
        .auth
        .as_deref()
        .ok_or_else(|| GatewayError::Unauthorized.into())
}

/// Create a virtual key.
pub async fn create_key(
    State(state): State<AppState>,
    payload: Result<Json<NewKey>, JsonRejection>,
) -> Result<(StatusCode, Json<CreatedKey>), ApiError> {
    let Json(params) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if params.name.trim().is_empty() {
        return Err(GatewayError::InvalidRequest("`name` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let (plaintext, record) = auth
        .store
        .create_key(params)
        .await
        .map_err(|e| internal(&e))?;
    // Make the key usable immediately, without waiting for a reboot.
    auth.keys.upsert(hash_key(plaintext.reveal()), &record);
    Ok((
        StatusCode::CREATED,
        Json(CreatedKey {
            key: plaintext.reveal().to_owned(),
            record,
        }),
    ))
}

/// List every key (no secrets: ids, names, budgets, limits, flags).
pub async fn list_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<VirtualKeyRecord>>, ApiError> {
    let auth = runtime(&state)?;
    let keys = auth.store.list_keys().await.map_err(|e| internal(&e))?;
    Ok(Json(keys))
}

/// Patch a key: adjust budgets/limits, enable/disable. An unknown id is a
/// 400 `LM-1001` naming the id — the public taxonomy reserves 404 for
/// unknown *models* (`LM-2001`) and has no admin-resource code.
pub async fn patch_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Result<Json<KeyPatch>, JsonRejection>,
) -> Result<Json<VirtualKeyRecord>, ApiError> {
    let Json(patch) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    let updated = auth
        .store
        .update_key(&id, patch)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    // Reflect the change in the live table (spend is preserved).
    auth.keys.apply(&updated);
    Ok(Json(updated))
}

/// `PUT /admin/provider-keys/{name}` body.
#[derive(Debug, Deserialize)]
pub struct ProviderKeyBody {
    /// The provider API key to seal. Never logged; encrypted at rest.
    pub key: String,
}

// The body deliberately has no Debug-derived secret exposure: ProviderKeyBody
// derives Debug for extractor plumbing but is never logged by the handler.

/// Store a provider key encrypted at rest. Takes effect at next boot (the
/// provider registry is built at startup; hot reload lands in M7).
pub async fn put_provider_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
    payload: Result<Json<ProviderKeyBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if body.key.trim().is_empty() {
        return Err(GatewayError::InvalidRequest("`key` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let Some(master) = auth.master.as_ref() else {
        return Err(GatewayError::Internal("master key unavailable".to_owned()).into());
    };
    auth.store
        .store_provider_key(&name, &body.key, master)
        .await
        .map_err(|e| internal(&e))?;
    Ok(StatusCode::NO_CONTENT)
}
