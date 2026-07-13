//! `POST /v1/rerank` — Cohere-format reranking.
//!
//! Flow: validate → route (model → provider) → admit (budget/quota, memory
//! only) → rerank (with gateway-side ordering, `top_n` clamping and optional
//! document echo) → account (search units, tokens, cost) → response. Like
//! embeddings, a per-request [`CancellationToken`] aborts the upstream call
//! if the client disconnects.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Extension, Json};
use ferrogate_core::{tokens, GatewayError, RerankResponse};
use ferrogate_providers::rerank;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::state::AppState;

/// Cohere's convention: one search unit covers a query over up to 100
/// documents. Used when the upstream bills otherwise (or not at all).
const DOCS_PER_SEARCH_UNIT: usize = 100;

/// Handle a rerank request.
pub async fn rerank_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<ferrogate_core::RerankRequest>, JsonRejection>,
) -> Result<Json<RerankResponse>, ApiError> {
    // Malformed body → FG-1001 in our envelope (not axum's plain-text default).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    // Empty documents is a distinct, pinned client error (FG-2010).
    if req.documents.is_empty() {
        return Err(GatewayError::EmptyDocuments.into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let client_model = req.model.clone();
    let route = ferrogate_router::resolve_rerank(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();

    // Admission BEFORE the upstream call (M5 §5.2). Rerank cost is billed in
    // search units; TPM counts the query × documents token estimate.
    let estimated_tokens = tokens::estimate_rerank(&req);
    let estimated_units = estimate_search_units(req.documents.len());
    let estimated_cost = state.pricing.search_cost(&client_model, estimated_units);
    let accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "rerank",
            model: &client_model,
            provider: &route.provider_name,
        },
        estimated_tokens,
        estimated_cost,
    )?;

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), aborting the in-flight upstream call.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    let mut response = rerank::rerank(route.provider.as_ref(), req, &cancel)
        .await
        .map_err(|e| GatewayError::from_provider(&route.provider_name, e))?;
    // (an early return above drops `accounting`, refunding the reservation)

    // ADR 003: upstream-billed search units when reported (Cohere), else the
    // gateway derives them from the batch size — never a silent zero.
    let (search_units, units_estimated) = if response.usage.search_units > 0 {
        (u64::from(response.usage.search_units), false)
    } else {
        (estimated_units, true)
    };
    if units_estimated {
        response.usage.search_units = u32::try_from(search_units).unwrap_or(u32::MAX);
        response.usage.estimated = Some(true);
    }
    let cost = state.pricing.search_cost(&client_model, search_units);
    accounting.finish(&Outcome {
        // Rerank tokens are always gateway-estimated (uniform observability
        // per ADR 003); the billing unit is `search_units`.
        tokens_in: estimated_tokens,
        tokens_out: 0,
        estimated: true,
        search_units: Some(search_units),
        cost,
        status: 200,
    });

    Ok(Json(response))
}

/// One search unit per (query × up-to-100-documents), never zero.
fn estimate_search_units(documents: usize) -> u64 {
    u64::try_from(documents.div_ceil(DOCS_PER_SEARCH_UNIT).max(1)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_unit_estimate_follows_cohere_convention() {
        assert_eq!(estimate_search_units(1), 1);
        assert_eq!(estimate_search_units(100), 1);
        assert_eq!(estimate_search_units(101), 2);
        assert_eq!(estimate_search_units(1000), 10);
    }
}
