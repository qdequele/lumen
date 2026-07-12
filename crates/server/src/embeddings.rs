//! `POST /v1/embeddings` — the first complete request path.
//!
//! Flow: validate → route (model → provider) → embed (with automatic batching)
//! → OpenAI-format response. A per-request [`CancellationToken`] is cancelled if
//! the client disconnects (via the drop guard), aborting the upstream call.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::Json;
use ferrogate_core::{EmbedResponse, GatewayError};
use ferrogate_providers::batch;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::AppState;

/// Handle an embeddings request.
pub async fn embeddings(
    State(state): State<AppState>,
    payload: Result<Json<ferrogate_core::EmbedRequest>, JsonRejection>,
) -> Result<Json<EmbedResponse>, ApiError> {
    // Malformed request body → FG-1001 in our standard envelope (not axum's
    // default plain-text rejection).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.input.is_empty() {
        return Err(GatewayError::InvalidRequest("`input` must not be empty".to_owned()).into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let route = ferrogate_router::resolve_embedding(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), cancelling the token and aborting the in-flight upstream
    // call so the provider stops work.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    let response = batch::embed_batched(
        route.provider.as_ref(),
        req,
        &cancel,
        batch::DEFAULT_CONCURRENCY,
    )
    .await
    .map_err(|e| GatewayError::from_provider(&route.provider_name, e))?;

    Ok(Json(response))
}
