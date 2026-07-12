//! `POST /v1/rerank` — Cohere-format reranking.
//!
//! Flow: validate → route (model → provider) → rerank (with gateway-side
//! ordering, `top_n` clamping and optional document echo) → response. Like
//! embeddings, a per-request [`CancellationToken`] aborts the upstream call if
//! the client disconnects.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::Json;
use ferrogate_core::{GatewayError, RerankResponse};
use ferrogate_providers::rerank;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::AppState;

/// Handle a rerank request.
pub async fn rerank_handler(
    State(state): State<AppState>,
    payload: Result<Json<ferrogate_core::RerankRequest>, JsonRejection>,
) -> Result<Json<RerankResponse>, ApiError> {
    // Malformed body → FG-1001 in our envelope (not axum's plain-text default).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    // Empty documents is a distinct, pinned client error (FG-2010).
    if req.documents.is_empty() {
        return Err(GatewayError::EmptyDocuments.into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let route = ferrogate_router::resolve_rerank(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), aborting the in-flight upstream call.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    let response = rerank::rerank(route.provider.as_ref(), req, &cancel)
        .await
        .map_err(|e| GatewayError::from_provider(&route.provider_name, e))?;

    Ok(Json(response))
}
