//! `POST /v1/systemone` - typed SystemOne decisions (TypeSafe Jev), ADR 013.
//!
//! Flow: validate (the documented question contract, so a malformed question
//! is a precise `LM-1001` rather than an opaque upstream `422`) → route
//! (model → provider, with fallbacks) → admit (budget/quota, memory only) →
//! evaluate → account (input/output tokens, cost) → response. The body is
//! TypeSafe's wire shape in both directions, so the TypeSafe SDKs work against
//! the gateway via `TYPESAFE_BASE_URL`. A per-request [`CancellationToken`]
//! aborts the upstream call if the client disconnects.
//!
//! `state`, questions and answers are request content: they are never logged
//! and never reach `usage_log`.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use lumen_core::{tokens, GatewayError, SystemOneUsage};
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target, TokenBreakdown};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::resilience::model_used_headers;
use crate::state::AppState;

/// Handle a SystemOne evaluation request.
pub async fn systemone_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<lumen_core::SystemOneRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → LM-1001 in our envelope (not axum's plain-text default).
    let Json(req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    // Empty questions → LM-2011; any other contract violation → LM-1001.
    req.validate()?;

    // Resolve the requested model to a fallback chain (M6 §6.2).
    let client_model = req.model.clone();
    let chain_ids = state.resilience.chain_ids(&client_model);
    let chain = lumen_router::resolve_systemone_chain(&state.registry, &chain_ids)?;
    let links = lumen_router::systemone_links(&chain);
    let exec = state.resilience.exec_config(&client_model);

    // Admission BEFORE the upstream call (M5 §5.2): reserve the input
    // estimate (state + every question); answers are tiny and Jev bills
    // input only, so no output tokens are reserved.
    let pricing = state.pricing();
    let estimated_input = tokens::estimate_systemone(&req);
    let estimated_cost = pricing.token_cost(&client_model, estimated_input, 0);
    let mut accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "systemone",
            model: &client_model,
            provider: &chain[0].route.provider_name,
        },
        estimated_input,
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
            async move { provider.evaluate(attempt_req, cancel).await }
        })
        .await?;
    // (an early return above drops `accounting`, refunding the reservation)

    let mut response = executed.value;
    accounting.served_by(&executed.model_used, &executed.provider_used);

    // ADR 003: upstream-reported usage wins; otherwise the gateway's input
    // estimate, flagged - never a silent zero. An upstream output count is
    // kept even when its input count is missing.
    let usage = settle_usage(response.usage, estimated_input);
    response.usage = Some(usage);
    let tokens_in = u64::from(usage.input_tokens);
    let tokens_out = u64::from(usage.output_tokens);
    let cost = pricing.token_cost(&executed.model_used, tokens_in, tokens_out);

    accounting.finish(&Outcome {
        tokens_in,
        tokens_out,
        estimated: usage.estimated == Some(true),
        search_units: None,
        breakdown: TokenBreakdown::default(),
        media: lumen_core::MediaUsage::default(),
        cost,
        status: 200,
    });

    Ok((model_used_headers(&executed.model_used), Json(response)).into_response())
}

/// The usage to report and account: upstream counts when the upstream
/// reported a non-zero input count, else the gateway estimate flagged
/// `estimated` (ADR 003).
fn settle_usage(upstream: Option<SystemOneUsage>, estimated_input: u64) -> SystemOneUsage {
    match upstream {
        Some(usage) if usage.input_tokens > 0 => SystemOneUsage {
            estimated: None,
            ..usage
        },
        other => SystemOneUsage {
            input_tokens: u32::try_from(estimated_input.max(1)).unwrap_or(u32::MAX),
            output_tokens: other.map_or(0, |u| u.output_tokens),
            estimated: Some(true),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_usage_wins_unflagged() {
        let usage = settle_usage(
            Some(SystemOneUsage {
                input_tokens: 300,
                output_tokens: 20,
                estimated: None,
            }),
            99,
        );
        assert_eq!((usage.input_tokens, usage.output_tokens), (300, 20));
        assert_eq!(usage.estimated, None);
    }

    #[test]
    fn missing_or_zero_usage_falls_back_to_the_flagged_estimate() {
        let missing = settle_usage(None, 42);
        assert_eq!((missing.input_tokens, missing.output_tokens), (42, 0));
        assert_eq!(missing.estimated, Some(true));

        let zero_input = settle_usage(
            Some(SystemOneUsage {
                input_tokens: 0,
                output_tokens: 7,
                estimated: None,
            }),
            42,
        );
        assert_eq!((zero_input.input_tokens, zero_input.output_tokens), (42, 7));
        assert_eq!(zero_input.estimated, Some(true));
    }

    #[test]
    fn the_estimate_is_never_zero() {
        assert_eq!(settle_usage(None, 0).input_tokens, 1);
    }
}
