//! `POST /v1/rerank` - Cohere-format reranking.
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
use lumen_core::{tokens, GatewayError};
use lumen_providers::rerank;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target, TokenBreakdown};
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
    payload: Result<Json<lumen_core::RerankRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → LM-1001 in our envelope (not axum's plain-text default).
    let Json(req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    // Empty documents is a distinct, pinned client error (LM-2010).
    if req.documents.is_empty() {
        return Err(GatewayError::EmptyDocuments.into());
    }

    // Resolve the requested model to a fallback chain (M6 §6.2).
    let client_model = req.model.clone();
    let chain_ids = state.resilience.chain_ids(&client_model);
    let chain = lumen_router::resolve_rerank_chain(&state.registry, &chain_ids)?;
    let links = lumen_router::rerank_links(&chain);
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

    let executed =
        lumen_router::executor::execute(&links, &state.resilience.breakers, &exec, &cancel, |i| {
            let provider = chain[i].route.provider.clone();
            let cancel = cancel.clone();
            let mut attempt_req = req.clone();
            chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            async move { rerank::rerank(provider.as_ref(), attempt_req, &cancel).await }
        })
        .await?;
    // (an early return above drops `accounting`, refunding the reservation)

    let mut response = executed.value;
    accounting.served_by(&executed.model_used, &executed.provider_used);

    // ADR 003: upstream-billed search units when reported (Cohere), else the
    // gateway derives them from the batch size - never a silent zero.
    let (search_units, units_estimated) = if response.usage.search_units > 0 {
        (u64::from(response.usage.search_units), false)
    } else {
        (estimated_units, true)
    };
    if units_estimated {
        response.usage.search_units = u32::try_from(search_units).unwrap_or(u32::MAX);
        response.usage.estimated = Some(true);
    }

    // ADR 003 / issue #10: upstream-reported token usage (Jina, Voyage) wins
    // when present; otherwise the gateway falls back to the query+documents
    // heuristic estimate - never a silent zero, and honestly flagged either
    // way.
    let (tokens_in, tokens_in_estimated) = if response.usage.total_tokens > 0 {
        (u64::from(response.usage.total_tokens), false)
    } else {
        (estimated_tokens, true)
    };
    if tokens_in_estimated {
        response.usage.total_tokens = u32::try_from(tokens_in).unwrap_or(u32::MAX);
        response.usage.tokens_estimated = Some(true);
    }

    // Cost is search-unit based and independent of the token count.
    let cost = pricing.search_cost(&executed.model_used, search_units);

    // Settle accounting (ADR 003). Upstream-reported token counts (Jina,
    // Voyage) settle inline, unflagged - there is nothing to refine. A
    // gateway-estimated count settles inline with the byte heuristic; when the
    // opt-in accurate tokenizer refines this model, the close is instead
    // deferred to a background task that recounts (query x documents) with
    // exact BPE on the blocking pool - the response is never delayed, only
    // usage_log/Prometheus gain precision. In practice rerank model ids rarely
    // match an OpenAI tiktoken family, so this refinement is usually a no-op.
    if tokens_in_estimated && state.token_counter.refines(&executed.model_used) {
        accounting.mark_completed();
        let counter = std::sync::Arc::clone(&state.token_counter);
        let model = executed.model_used.clone();
        let query = req.query.clone();
        let docs: Vec<String> = req.documents.iter().map(|d| d.text().to_owned()).collect();
        tokio::spawn(async move {
            let tokens_in = counter
                .refine_rerank(&model, query, docs)
                .await
                .unwrap_or(estimated_tokens);
            accounting.finish(&Outcome {
                tokens_in,
                tokens_out: 0,
                estimated: true,
                search_units: Some(search_units),
                breakdown: TokenBreakdown::default(),
                media: lumen_core::MediaUsage::default(),
                cost,
                status: 200,
            });
        });
    } else {
        accounting.finish(&Outcome {
            tokens_in,
            tokens_out: 0,
            estimated: tokens_in_estimated,
            search_units: Some(search_units),
            breakdown: TokenBreakdown::default(),
            media: lumen_core::MediaUsage::default(),
            cost,
            status: 200,
        });
    }

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
