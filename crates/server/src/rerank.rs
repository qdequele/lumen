//! `POST /v1/rerank` - Cohere-format reranking.
//!
//! Flow: validate → route (model → provider) → admit (budget/quota, memory
//! only) → rerank (with gateway-side ordering, `top_n` clamping and optional
//! document echo) → account (search units, tokens, cost) → response. A
//! `typesafe` rerank model is served by Jev through a converter (ADR 013
//! amendment); nothing here is specific to it beyond token pricing. Like
//! embeddings, a per-request [`CancellationToken`] aborts the upstream call
//! if the client disconnects.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use lumen_core::{tokens, Capability, GatewayError, ProviderError};
use lumen_providers::rerank;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target, TokenBreakdown};
use crate::auth::AuthedKey;
use crate::bandwidth::UpstreamMark;
use crate::byok::{RequestKeys, Served};
use crate::error::{body_rejection, ApiError};
use crate::facts::Facts;
use crate::resilience::routing_headers;
use crate::state::AppState;

/// Cohere's convention: one search unit covers a query over up to 100
/// documents. Used when the upstream bills otherwise (or not at all).
const DOCS_PER_SEARCH_UNIT: usize = 100;

/// Handle a rerank request.
// One straight-line request pipeline; the team-key routing of each attempt
// (BYOK) tipped it over the limit.
#[allow(clippy::too_many_lines)]
pub async fn rerank_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    Extension(mark): Extension<UpstreamMark>,
    payload: Result<Json<lumen_core::RerankRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → LM-1001 in our envelope (not axum's plain-text
    // default); a body over the limit → LM-1002.
    let Json(req) = payload.map_err(|e| body_rejection(&e, state.body_limit))?;

    // Empty documents is a distinct, pinned client error (LM-2010).
    if req.documents.is_empty() {
        return Err(GatewayError::EmptyDocuments.into());
    }

    // A deprecated bare id (`gpt-4o`) resolves to its `provider/model` id.
    // An account key also resolves its account's own virtual models (ADR 022).
    let account = key.as_deref().and_then(|k| k.0.account_ref());
    let account = account.as_deref().map(String::as_str);
    let client_model = state.resilience.canonical_model_for(account, &req.model);
    // Facts are only built for a virtual model (ADR 014).
    let mut decision =
        state
            .resilience
            .decide_for(Capability::Rerank, account, &client_model, |_| {
                Facts::rerank(&headers, key.as_deref(), &req)
            })?;
    let refusals = Some(&state.decision_metrics); // refused remap attempts (ADR 017)
    let chain = lumen_router::resolve_rerank_decision(&state.registry, &mut decision, refusals)?;
    let primary = decision.primary_model().to_owned();
    let mut links = lumen_router::decision_links(
        &decision,
        chain.iter().map(|l| l.route.provider_name.as_str()),
    );
    let exec = state.resilience.exec_config(&primary);
    // Each link is retried on its own provider's policy (ADR 021).
    state.resilience.apply_retry_overrides(&mut links);
    // Admission BEFORE the upstream call (M5 §5.2). Rerank cost is billed in
    // search units; TPM counts the query × documents token estimate.
    let pricing = state.pricing();
    let estimated_tokens = tokens::estimate_rerank(&req);
    let estimated_units = estimate_search_units(req.documents.len());
    // Search-unit price plus, for token-billed rerankers (Jev, and any model
    // pricing `cost_per_1m_input`), the token price.
    // A primary served by the group's own keys (BYOK) reserves no cost.
    let keys = RequestKeys::new(&state.team_pools, key.as_deref()).for_chain(
        &state.registry,
        chain.iter().map(|l| l.route.provider_name.as_str()),
    );
    // Links served from a team pool get their own breaker (ADR 018).
    keys.scope_links(&state.registry, &mut links);
    let estimated_cost = if keys.has_usable_pool(&chain[0].route.provider_name) {
        0.0
    } else {
        pricing.search_cost(&primary, estimated_units)
            + pricing.token_cost(&primary, estimated_tokens, 0)
    };
    let mut accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "rerank",
            model: &client_model,
            provider: &chain[0].route.provider_name,
            account_model: decision.account,
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
            let platform = chain[i].route.provider.clone();
            let provider_name = chain[i].route.provider_name.clone();
            let cancel = cancel.clone();
            let mut attempt_req = req.clone();
            decision.attempts[i].apply(&mut attempt_req);
            chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            let (keys, registry, mark) = (&keys, &*state.registry, &mark);
            async move {
                // Marked only when an upstream call is actually sent.
                let call = |provider: Option<Arc<dyn lumen_core::RerankProvider>>| {
                    let (req, cancel, name) =
                        (attempt_req.clone(), cancel.clone(), provider_name.clone());
                    async move {
                        let provider =
                            provider.ok_or(ProviderError::Unavailable { provider: name })?;
                        mark.mark();
                        rerank::rerank(provider.as_ref(), req, &cancel).await
                    }
                };
                keys.attempt(
                    registry,
                    i,
                    &provider_name,
                    || call(Some(platform)),
                    |set| call(set.rerank.clone()),
                )
                .await
            }
        })
        .await
        // Every attempt failed: still leave a status-only usage row.
        .map_err(|e| accounting.fail(e))?;
    // (an early return above drops `accounting`, refunding the reservation)

    let (mut response, served) = executed.value;
    let billable = matches!(served, Served::Platform);
    accounting.served_by(&executed.model_used, &executed.provider_used);
    // Refusals in a served remap response (ADR 017); 0 for native rerankers.
    state
        .decision_metrics
        .add_refusals(&executed.model_used, u64::from(response.usage.refusals));
    let route = decision.route_of(executed.index);
    accounting.set_route(route, decision.account);

    let search_units = settle_search_units(&mut response, estimated_units);
    let (tokens_in, tokens_in_estimated) = settle_tokens(&mut response, estimated_tokens);

    // Cost: search units, plus input tokens for models priced per token
    // (`cost_per_1m_input`; zero for the search-unit-only rerankers).
    let cost = pricing.search_cost(&executed.model_used, search_units)
        + pricing.token_cost(&executed.model_used, tokens_in, 0);

    // Settle accounting (ADR 003). Upstream-reported token counts (Jina,
    // Voyage) settle inline, unflagged - there is nothing to refine. A
    // gateway-estimated count settles inline with the byte heuristic; when the
    // opt-in accurate tokenizer refines this model, the close is instead
    // deferred to a background task that recounts (query x documents) with
    // exact BPE on the blocking pool - the response is never delayed, only
    // usage_log/Prometheus gain precision. In practice rerank model ids rarely
    // match an OpenAI tiktoken family, so this refinement is usually a no-op.
    if tokens_in_estimated && state.token_counter().refines(&executed.model_used) {
        accounting.mark_completed();
        let counter = state.token_counter();
        let model = executed.model_used.clone();
        let query = req.query.clone();
        // The text the provider ranked (the `rank_fields` reduction).
        let rank_fields = req.rank_fields.as_deref();
        let docs: Vec<String> = req
            .documents
            .iter()
            .map(|d| d.rank_text(rank_fields).into_owned())
            .collect();
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
                billable,
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
            billable,
        });
    }

    Ok((routing_headers(&executed.model_used, route), Json(response)).into_response())
}

/// ADR 003: upstream-billed search units when reported (Cohere), else the
/// gateway derives them from the batch size - never a silent zero.
fn settle_search_units(response: &mut lumen_core::RerankResponse, estimated_units: u64) -> u64 {
    if response.usage.search_units > 0 {
        return u64::from(response.usage.search_units);
    }
    response.usage.search_units = u32::try_from(estimated_units).unwrap_or(u32::MAX);
    response.usage.estimated = Some(true);
    estimated_units
}

/// ADR 003 / issue #10: upstream-reported token usage (Jina, Voyage) wins
/// when present; otherwise the gateway falls back to the query+documents
/// heuristic estimate - never a silent zero, and honestly flagged either way.
/// Returns the token count to account and whether it is an estimate.
fn settle_tokens(response: &mut lumen_core::RerankResponse, estimated_tokens: u64) -> (u64, bool) {
    if response.usage.total_tokens > 0 {
        return (u64::from(response.usage.total_tokens), false);
    }
    response.usage.total_tokens = u32::try_from(estimated_tokens).unwrap_or(u32::MAX);
    response.usage.tokens_estimated = Some(true);
    (estimated_tokens, true)
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
