//! `GET /v1/models` and `GET /v1/models/{id}` - model discovery.
//!
//! Lists every model the operator configured, in the OpenAI list shape extended
//! with a `capabilities` array, and serves single-model retrieval from the same
//! registry snapshot. Both routes reflect ONLY the local configuration - the
//! gateway never introspects upstreams (spec 3.3), so they touch no provider
//! and do no I/O.

use axum::extract::{Path, State};
use axum::Json;
use lumen_core::GatewayError;
use lumen_providers::LoadedModelSummary;
use serde::Serialize;

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
    /// The provider that owns this model.
    pub owned_by: String,
    /// Capabilities this model serves (`chat` / `embed` / `rerank`).
    pub capabilities: Vec<&'static str>,
    /// Input modalities this model accepts (`text`, `image`).
    pub modalities: Vec<String>,
}

impl From<LoadedModelSummary> for ModelEntry {
    fn from(m: LoadedModelSummary) -> Self {
        ModelEntry {
            id: m.id,
            object: "model",
            owned_by: m.owned_by,
            capabilities: m.capabilities.iter().map(|c| c.as_str()).collect(),
            modalities: m.modalities,
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

/// Handle a model-discovery request.
#[allow(clippy::unused_async)] // axum handlers are async; this one just reads state.
pub async fn models(State(state): State<AppState>) -> Json<ModelList> {
    let data = state
        .registry
        .list_models()
        .into_iter()
        .map(ModelEntry::from)
        .collect();

    Json(ModelList {
        object: "list",
        data,
    })
}

/// Handle a single-model retrieve request (`GET /v1/models/{id}`).
///
/// Returns the exact per-model object the list emits, from the same registry
/// snapshot. An unknown id is a 404 with the `LM-2001` envelope - the same
/// taxonomy entry the routing layer uses for unknown models.
#[allow(clippy::unused_async)] // axum handlers are async; this one just reads state.
pub async fn model(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ModelEntry>, ApiError> {
    state
        .registry
        .model(&id)
        .map(|m| Json(ModelEntry::from(m)))
        .ok_or_else(|| ApiError::from(GatewayError::ModelNotFound(id)))
}
