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
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use ferrogate_core::{tokens, GatewayError};
use ferrogate_providers::rerank;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::resilience::model_used_headers;
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
) -> Result<Response, ApiError> {
    // Malformed body → FG-1001 in our envelope (not axum's plain-text default).
    let Json(req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    // Empty documents is a distinct, pinned client error (FG-2010).
    if req.documents.is_empty() {
        return Err(GatewayError::EmptyDocuments.into());
    }

    // Resolve the requested model to a fallback chain (M6 §6.2).
    let client_model = req.model.clone();
    let chain_ids = state.resilience.chain_ids(&client_model);
    let chain = ferrogate_router::resolve_rerank_chain(&state.registry, &chain_ids)?;
    let links = ferrogate_router::rerank_links(&chain);
    let exec = state.resilience.exec_config(&client_model);

    // Admission BEFORE the upstream call (M5 §5.2). Rerank cost is billed in
    // search units; TPM counts the query × documents token estimate.
    let pricing = state.pricing();
    let estimated_tokens = tokens::estimate_rerank(&req);
    let estimated_units = estimate_search_units(req.documents.len());
    let estimated_cost = pricing.search_cost(&client_model, estimated_units);
    let mut accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "rerank",
            model: &client_model,
            provider: &chain[0].route.provider_name,
        },
        estimated_tokens,
        estimated_cost,
        pricing.clone(),
    )?;

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), aborting the in-flight upstream call.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    let executed = ferrogate_router::executor::execute(
        &links,
        &state.resilience.breakers,
        &exec,
        &cancel,
        |i| {
            let provider = chain[i].route.provider.clone();
            let cancel = cancel.clone();
            let mut attempt_req = req.clone();
            chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            async move { rerank::rerank(provider.as_ref(), attempt_req, &cancel).await }
        },
    )
    .await?;
    // (an early return above drops `accounting`, refunding the reservation)

    let mut response = executed.value;
    accounting.served_by(&executed.model_used, &executed.provider_used);

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
    let cost = pricing.search_cost(&executed.model_used, search_units);
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

    Ok((model_used_headers(&executed.model_used), Json(response)).into_response())
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
