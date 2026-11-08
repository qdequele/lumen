//! `GET /v1/models` and `GET /v1/models/{id}` - model discovery.
//!
//! Lists every model the operator configured (the foundation models first,
//! then the listed virtual models, ADR 014), in the OpenAI list shape extended
//! with a `capabilities` array (and, when declared, a `release_date` mirrored
//! into OpenAI's `created`), and serves single-model retrieval from the same
//! snapshots. Both routes reflect ONLY the local configuration - the gateway
//! never introspects upstreams (spec 3.3), so they touch no provider and do no
//! I/O. A virtual model with `listed = false` is hidden from both routes.
//!
//! A key of a Lab account also sees that account's own listed virtual models
//! (ADR 022, `"owned_by": "account"`), and an account model hides a platform
//! virtual model of the same id from that key. Other keys see the platform
//! only.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use lumen_core::{GatewayError, ReleaseDate};
use lumen_providers::LoadedModelSummary;
use serde::Serialize;

use crate::account_models::AccountModels;
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::state::AppState;

/// One entry in the `GET /v1/models` list, and the whole body of
/// `GET /v1/models/{id}` - the two routes share this type so their per-model
/// shape can never diverge.
#[derive(Debug, Serialize)]
pub struct ModelEntry {
    /// Client-facing model id.
    pub id: String,
    /// Always `"model"` (OpenAI compatibility).
    pub object: &'static str,
    /// Unix seconds at midnight UTC of the release date (the OpenAI `created`
    /// field). Omitted when the operator declared no `release_date`, rather
    /// than reporting a misleading epoch 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    /// The provider that owns this model.
    pub owned_by: String,
    /// Capabilities this model serves (`chat` / `embed` / `rerank` / `decisions`).
    pub capabilities: Vec<&'static str>,
    /// Input modalities this model accepts (`text`, `image`).
    pub modalities: Vec<String>,
    /// Operator-declared release date, ISO 8601 `YYYY-MM-DD`. Omitted when
    /// unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<ReleaseDate>,
    /// Operator description (virtual models, ADR 014). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this id is a virtual model (ADR 014).
    #[serde(rename = "virtual")]
    pub is_virtual: bool,
}

impl From<LoadedModelSummary> for ModelEntry {
    fn from(m: LoadedModelSummary) -> Self {
        ModelEntry {
            id: m.id,
            object: "model",
            created: m.release_date.map(ReleaseDate::unix_seconds),
            owned_by: m.owned_by,
            capabilities: m.capabilities.iter().map(|c| c.as_str()).collect(),
            modalities: m.modalities,
            release_date: m.release_date,
            description: None,
            is_virtual: false,
        }
    }
}

impl From<&lumen_router::virtual_models::VirtualModel> for ModelEntry {
    fn from(m: &lumen_router::virtual_models::VirtualModel) -> Self {
        ModelEntry {
            id: m.id().to_owned(),
            object: "model",
            created: None,
            owned_by: if m.is_account() { "account" } else { "lumen" }.to_owned(),
            capabilities: vec![m.capability().as_str()],
            modalities: m.modalities().to_vec(),
            release_date: None,
            description: m.description().map(str::to_owned),
            is_virtual: true,
        }
    }
}

/// The `GET /v1/models` envelope.
#[derive(Debug, Serialize)]
pub struct ModelList {
    /// Always `"list"`.
    pub object: &'static str,
    /// The configured models.
    pub data: Vec<ModelEntry>,
}

/// The calling key's account models (ADR 022), when it has an account with
/// models stored.
fn account_models(
    state: &AppState,
    key: Option<&Extension<AuthedKey>>,
) -> Option<std::sync::Arc<AccountModels>> {
    let account = key.and_then(|k| k.0 .0.account_ref())?;
    state.resilience.account_models(&account)
}

/// Whether `id` is declared by the account (served or disabled), so the
/// platform model of the same id is hidden from its keys.
fn shadowed(account: Option<&AccountModels>, id: &str) -> bool {
    account.is_some_and(|a| a.table.get(id).is_some() || a.table.is_disabled(id))
}

/// Handle a model-discovery request.
#[allow(clippy::unused_async)] // axum handlers are async; this one just reads state.
pub async fn models(
    State(state): State<AppState>,
    key: Option<Extension<AuthedKey>>,
) -> Json<ModelList> {
    let account = account_models(&state, key.as_ref());
    let account = account.as_deref();
    let mut data: Vec<ModelEntry> = state
        .registry
        .list_models()
        .into_iter()
        .filter(|m| !shadowed(account, &m.id))
        .map(ModelEntry::from)
        .collect();
    data.extend(
        state
            .resilience
            .routing()
            .listed()
            .filter(|m| !shadowed(account, m.id()))
            .map(|m| ModelEntry::from(m.as_ref())),
    );
    if let Some(account) = account {
        data.extend(account.table.listed().map(|m| ModelEntry::from(m.as_ref())));
    }

    Json(ModelList {
        object: "list",
        data,
    })
}

/// Handle a single-model retrieve request (`GET /v1/models/{id}`).
///
/// Returns the exact per-model object the list emits, from the same registry
/// snapshot (a listed virtual model is served as a fallback; a hidden one is a
/// 404). An unknown id is a 404 with the `LM-2001` envelope - the same
/// taxonomy entry the routing layer uses for unknown models.
#[allow(clippy::unused_async)] // axum handlers are async; this one just reads state.
pub async fn model(
    State(state): State<AppState>,
    key: Option<Extension<AuthedKey>>,
    Path(id): Path<String>,
) -> Result<Json<ModelEntry>, ApiError> {
    // The account's own model wins (ADR 022); a disabled or unlisted one is
    // a 404 like an unknown id, never the platform model it shadows.
    if let Some(account) = account_models(&state, key.as_ref()) {
        if shadowed(Some(&account), &id) {
            return account
                .table
                .get(&id)
                .filter(|m| m.listed())
                .map(|m| Json(ModelEntry::from(m.as_ref())))
                .ok_or_else(|| ApiError::from(GatewayError::ModelNotFound(id)));
        }
    }
    // A deprecated bare id is served as the entry of the id it aliases,
    // unless the account declares that id (as the request path resolves it).
    let account = account_models(&state, key.as_ref());
    let id = state.resilience.canonical_model(&id);
    if let Some(account) = account.filter(|a| shadowed(Some(a), &id)) {
        return account
            .table
            .get(&id)
            .filter(|m| m.listed())
            .map(|m| Json(ModelEntry::from(m.as_ref())))
            .ok_or_else(|| ApiError::from(GatewayError::ModelNotFound(id)));
    }
    state
        .registry
        .model(&id)
        .map(|m| Json(ModelEntry::from(m)))
        .or_else(|| {
            state
                .resilience
                .routing()
                .get(&id)
                .filter(|m| m.listed())
                .map(|m| Json(ModelEntry::from(m.as_ref())))
        })
        .ok_or_else(|| ApiError::from(GatewayError::ModelNotFound(id)))
}
