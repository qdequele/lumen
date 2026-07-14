//! Operational routes: `/health` and `/metrics`.
//!
//! Neither route touches the database or any provider. `/health` in particular
//! performs NO I/O - it answers 200 as long as the process is alive, so that a
//! readiness probe never fails under load and triggers a restart cascade
//! (lesson: LiteLLM #15526).

use crate::state::AppState;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;

/// Liveness/readiness probe. Always 200 `{"status":"ok"}` if the process runs.
pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// Prometheus metrics in the text exposition format.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics.encode_text();
    ([(header::CONTENT_TYPE, state.metrics.content_type())], body)
}
