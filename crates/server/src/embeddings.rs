//! `POST /v1/embeddings` — the first complete request path.
//!
//! Flow: validate → route (model → provider) → admit (budget/quota, memory
//! only) → embed (with automatic batching) → account (tokens, cost, usage
//! log) → OpenAI-format response. A per-request [`CancellationToken`] is
//! cancelled if the client disconnects (via the drop guard), aborting the
//! upstream call.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Extension, Json};
use ferrogate_core::{tokens, EmbedResponse, GatewayError};
use ferrogate_providers::batch;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::state::AppState;

/// Handle an embeddings request.
pub async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<ferrogate_core::EmbedRequest>, JsonRejection>,
) -> Result<Json<EmbedResponse>, ApiError> {
    // Malformed request body → FG-1001 in our standard envelope (not axum's
    // default plain-text rejection).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.input.is_empty() {
        return Err(GatewayError::InvalidRequest("`input` must not be empty".to_owned()).into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let client_model = req.model.clone();
    let route = ferrogate_router::resolve_embedding(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();

    // Admission BEFORE the upstream call: the pre-call estimate is reserved
    // atomically against the key's budget and quotas (M5 §5.2).
    let estimated_input = tokens::estimate_embed_input(&req);
    let estimated_cost = state.pricing.token_cost(&client_model, estimated_input, 0);
    let accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "embed",
            model: &client_model,
            provider: &route.provider_name,
        },
        estimated_input,
        estimated_cost,
    )?;

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), cancelling the token and aborting the in-flight upstream
    // call so the provider stops work.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    let mut response = batch::embed_batched(
        route.provider.as_ref(),
        req,
        &cancel,
        batch::DEFAULT_CONCURRENCY,
    )
    .await
    .map_err(|e| GatewayError::from_provider(&route.provider_name, e))?;
    // (an early return above drops `accounting`, refunding the reservation)

    // ADR 003: upstream usage when reported, else the local estimate — never
    // a silent zero (e.g. TEI reports nothing).
    let (tokens_in, estimated) = if response.usage.prompt_tokens > 0 {
        (u64::from(response.usage.prompt_tokens), false)
    } else {
        (estimated_input, true)
    };
    if estimated {
        // Surface the estimate in the response too (flagged, per ADR 003).
        response.usage.prompt_tokens = u32::try_from(tokens_in).unwrap_or(u32::MAX);
        response.usage.total_tokens = response.usage.prompt_tokens;
        response.usage.estimated = Some(true);
    }
    let cost = state.pricing.token_cost(&client_model, tokens_in, 0);
    accounting.finish(&Outcome {
        tokens_in,
        tokens_out: 0,
        estimated,
        search_units: None,
        cost,
        status: 200,
    });

    Ok(Json(response))
}
