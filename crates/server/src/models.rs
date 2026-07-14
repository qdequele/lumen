//! `GET /v1/models` — model discovery.
//!
//! Lists every model the operator configured, in the OpenAI list shape extended
//! with a `capabilities` array. It reflects ONLY the local configuration — the
//! gateway never introspects upstreams (spec 3.3), so this route touches no
//! provider and does no I/O.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// One entry in the `GET /v1/models` list.
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
        .map(|m| ModelEntry {
            id: m.id,
            object: "model",
            owned_by: m.owned_by,
            capabilities: m.capabilities.iter().map(|c| c.as_str()).collect(),
            modalities: m.modalities.clone(),
        })
        .collect();

    Json(ModelList {
        object: "list",
        data,
    })
}
