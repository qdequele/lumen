//! `POST /v1/decisions` (ADR 017) and its deprecated alias `/v1/systemone`.
//!
//! Flow: detect the format (OpenAI or TypeSafe) → parse and validate → route
//! (model → provider chain, incompatible targets skipped before any call) →
//! admit (budget/quota, memory only) → execute (retry/fallback) → settle
//! (ADR 003 tokens, cost) → render in the client's format. A per-request
//! [`CancellationToken`] aborts the upstream call on client disconnect.
//!
//! Input, images, questions, answers and `safety_identifier` are request
//! content: never logged and never in `usage_log`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use lumen_core::decisions::format::{self, Format};
use lumen_core::{tokens, Answer, Capability, DecisionUsage};
use lumen_telemetry::DecisionMetrics;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target, TokenBreakdown};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::facts::Facts;
use crate::resilience::routing_headers;
use crate::state::AppState;

/// RFC 9745 `Deprecation` value for `/v1/systemone`: the deprecating
/// release (0.6.0), as `@<unix seconds>` (2026-10-08 00:00 UTC; updated to
/// the release date when 0.6.0 is cut).
pub const SYSTEMONE_DEPRECATION: &str = "@1791417600";

/// RFC 8288 `Link` to the migration guide, sent with [`SYSTEMONE_DEPRECATION`].
const SYSTEMONE_LINK: &str = r#"</docs/decisions#migrating-from-v1systemone>; rel="deprecation""#;

/// Unix second of the last deprecation warning (one per minute at most).
static LAST_DEPRECATION_WARNING: AtomicU64 = AtomicU64::new(0);

/// `POST /v1/decisions`: either format, answered in the format received.
pub async fn decisions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(state, &headers, key.as_deref(), &body, None).await
}

/// `POST /v1/systemone` (deprecated in 0.6.0, removed in 0.7.0): the
/// TypeSafe format only, on the same pipeline. The `Deprecation` / `Link`
/// headers and the counter are applied by [`systemone_deprecation`], outside
/// auth and the body limit, so rejected requests carry them too.
pub async fn systemone_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(
        state,
        &headers,
        key.as_deref(),
        &body,
        Some(Format::TypeSafe),
    )
    .await
}

/// The deprecated route's path.
pub const SYSTEMONE_PATH: &str = "/v1/systemone";

/// Outer middleware for [`SYSTEMONE_PATH`] (spec 6.4): counts each request
/// once, logs the rate-limited warning, and puts `Deprecation` and `Link` on
/// every response, including auth (`LM-4004`, 429) and body-limit
/// (`LM-1002`) rejections that never reach the handler. Other paths pass
/// through untouched.
///
/// Its state is only the [`DecisionMetrics`] handle behind an [`Arc`] (one
/// refcount bump per request), not the whole [`AppState`].
pub async fn systemone_deprecation(
    State(metrics): State<Arc<DecisionMetrics>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() != SYSTEMONE_PATH {
        return next.run(request).await;
    }
    metrics.inc_deprecated(SYSTEMONE_PATH);
    warn_deprecated_use();
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(
        header::HeaderName::from_static("deprecation"),
        HeaderValue::from_static(SYSTEMONE_DEPRECATION),
    );
    h.insert(header::LINK, HeaderValue::from_static(SYSTEMONE_LINK));
    response
}

/// Log the deprecated-route warning at most once per minute (process-wide).
fn warn_deprecated_use() {
    let now = crate::auth::now_unix().max(0).unsigned_abs();
    let last = LAST_DEPRECATION_WARNING.load(Ordering::Relaxed);
    if (last == 0 || now.saturating_sub(last) >= 60)
        && LAST_DEPRECATION_WARNING
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            route = "/v1/systemone",
            "deprecated route used: send the same body to /v1/decisions (removed in 0.7.0)"
        );
    }
}

async fn handle(
    state: AppState,
    headers: &HeaderMap,
    key: Option<&AuthedKey>,
    body: &[u8],
    forced: Option<Format>,
) -> Result<Response, ApiError> {
    // Malformed JSON, an undetectable format or a contract violation →
    // LM-1001; empty questions → LM-2011; all before any routing.
    let (format, req) = format::parse(body, forced)?;
    let client_model = req.model.clone();
    // Facts are only built for a virtual model (ADR 014).
    let mut decision = state
        .resilience
        .decide(Capability::Decisions, &client_model, |_| {
            Facts::decisions(headers, key)
        })?;
    // Incompatible targets leave the decision here, before admission: they
    // are not attempts (spec 7.3).
    let chain = lumen_router::resolve_decisions(&state.registry, &mut decision, &req)?;
    let Some(first) = chain.first() else {
        return Err(lumen_core::GatewayError::Internal("empty decisions chain".to_owned()).into());
    };
    let primary = decision.primary_model().to_owned();
    let links = lumen_router::decision_links(
        &decision,
        chain.iter().map(|l| l.route.provider_name.as_str()),
    );
    let exec = state.resilience.exec_config(&primary);
    // Admission BEFORE the upstream call (M5 §5.2): reserve the input
    // estimate; decisions bill input only, so no output is reserved.
    let pricing = state.pricing();
    let estimated_input = tokens::estimate_decisions(&req);
    let estimated_cost = pricing.token_cost(&primary, estimated_input, 0);
    let mut accounting = Accounting::begin(
        &state,
        headers,
        key,
        Target {
            capability: "decisions",
            model: &client_model,
            provider: &first.route.provider_name,
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
            // A refcount bump: the request body is shared.
            let mut attempt = req.clone();
            chain[i].route.upstream_id.clone_into(&mut attempt.model);
            async move { provider.decide(attempt, cancel).await }
        })
        .await?;
    // (an early return above drops `accounting`, refunding the reservation)

    let mut response = executed.value;
    accounting.served_by(&executed.model_used, &executed.provider_used);
    let route = decision.route_of(executed.index);
    accounting.set_route(route);

    // ADR 003: upstream-reported usage wins; otherwise the gateway's input
    // estimate, flagged - never a silent zero.
    let usage = settle_usage(response.usage, estimated_input);
    response.usage = Some(usage);
    let refusals = response
        .answers
        .iter()
        .filter(|a| matches!(a, Answer::Refusal))
        .count();
    state.decision_metrics.add_refusals(
        &executed.model_used,
        u64::try_from(refusals).unwrap_or(u64::MAX),
    );
    let tokens_in = u64::from(usage.input_tokens);
    let tokens_out = u64::from(usage.output_tokens);
    let cost = pricing.token_cost(&executed.model_used, tokens_in, tokens_out);
    // Render before settling: a render failure returns a 500 and drops
    // `accounting` (refunding the reservation), never billing an error.
    let bytes = format::render(format, &response, &req)?;
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

    let mut out_headers = routing_headers(&executed.model_used, route);
    out_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok((out_headers, bytes).into_response())
}

/// The usage to report and account: upstream counts when the upstream
/// reported a non-zero input count, else the gateway estimate flagged
/// `estimated` (ADR 003). An upstream output count is kept even when its
/// input count is missing; cached and reasoning counts pass through.
fn settle_usage(upstream: Option<DecisionUsage>, estimated_input: u64) -> DecisionUsage {
    match upstream {
        Some(usage) if usage.input_tokens > 0 => DecisionUsage {
            estimated: None,
            ..usage
        },
        other => DecisionUsage {
            input_tokens: u32::try_from(estimated_input.max(1)).unwrap_or(u32::MAX),
            output_tokens: other.map_or(0, |u| u.output_tokens),
            estimated: Some(true),
            ..DecisionUsage::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_usage_wins_unflagged() {
        let usage = settle_usage(
            Some(DecisionUsage {
                input_tokens: 300,
                output_tokens: 20,
                cached_tokens: 4,
                reasoning_tokens: 2,
                estimated: None,
            }),
            99,
        );
        assert_eq!((usage.input_tokens, usage.output_tokens), (300, 20));
        assert_eq!((usage.cached_tokens, usage.reasoning_tokens), (4, 2));
        assert_eq!(usage.estimated, None);
    }

    #[test]
    fn missing_or_zero_usage_falls_back_to_the_flagged_estimate() {
        let missing = settle_usage(None, 42);
        assert_eq!((missing.input_tokens, missing.output_tokens), (42, 0));
        assert_eq!(missing.estimated, Some(true));

        let zero_input = settle_usage(
            Some(DecisionUsage {
                input_tokens: 0,
                output_tokens: 7,
                ..DecisionUsage::default()
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
