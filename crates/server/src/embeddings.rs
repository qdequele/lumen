//! `POST /v1/embeddings` - the first complete request path.
//!
//! Flow: validate → route (model → provider) → admit (budget/quota, memory
//! only) → embed (with automatic batching) → account (tokens, cost, usage
//! log) → OpenAI-format response. A per-request [`CancellationToken`] is
//! cancelled if the client disconnects (via the drop guard), aborting the
//! upstream call.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use lumen_core::{tokens, GatewayError};
use lumen_providers::batch;
use tokio_util::sync::CancellationToken;

use crate::accounting::{Accounting, Outcome, Target, TokenBreakdown};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::resilience::model_used_headers;
use crate::state::AppState;

/// Validate a Cohere `input_type` override (issue #22) up front, regardless of
/// which provider ultimately serves the request, so an unknown value fails fast
/// with LM-1001 rather than surfacing as an opaque upstream 400 (or, for a
/// non-Cohere provider, being silently ignored while the caller believes it
/// took effect).
fn validate_input_type(req: &lumen_core::EmbedRequest) -> Result<(), GatewayError> {
    if let Some(value) = req.extra.get("input_type") {
        let is_allowed = value
            .as_str()
            .is_some_and(|s| lumen_providers::cohere::ALLOWED_INPUT_TYPES.contains(&s));
        if !is_allowed {
            return Err(GatewayError::InvalidRequest(format!(
                "invalid `input_type` {value}: expected one of {:?}",
                lumen_providers::cohere::ALLOWED_INPUT_TYPES
            )));
        }
    }
    Ok(())
}

/// Handle an embeddings request.
pub async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<lumen_core::EmbedRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed request body → LM-1001 in our standard envelope (not axum's
    // default plain-text rejection).
    let Json(req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.input.is_empty() {
        return Err(GatewayError::InvalidRequest("`input` must not be empty".to_owned()).into());
    }

    validate_input_type(&req)?;

    // Resolve the requested model to a fallback chain (primary + configured
    // fallbacks), each re-resolved for the embed capability (M6 §6.2).
    let client_model = req.model.clone();
    let chain_ids = state.resilience.chain_ids(&client_model);
    let chain = lumen_router::resolve_embedding_chain(&state.registry, &chain_ids)?;
    let links = lumen_router::embedding_links(&chain);
    let exec = state.resilience.exec_config(&client_model);

    // M9 enforcement (fail fast): image input requires EVERY model in the
    // resolved chain (primary + fallbacks) to declare the "image" modality,
    // otherwise a fallback hop could route image content to a text-only model.
    // Rejected before any upstream call with a clear LM-2003 naming the
    // offending model. Shares `ImageInputNotSupported` with chat vision (M8).
    if req.input.has_image() {
        if let Some(bad) = chain_ids.iter().find(|id| {
            !state
                .registry
                .modalities(id)
                .is_some_and(|mods| mods.iter().any(|m| m == "image"))
        }) {
            return Err(GatewayError::ImageInputNotSupported { model: bad.clone() }.into());
        }
    }

    // Admission BEFORE the upstream call: the pre-call estimate is reserved
    // atomically against the key's budget and quotas (M5 §5.2). The provider
    // label is corrected to the one that actually serves (M6) after execution.
    // One consistent price snapshot for the whole request (a mid-request hot
    // reload can't shift prices between estimate and settlement).
    let pricing = state.pricing();
    let estimated_input = tokens::estimate_embed_input(&req);
    let estimated_cost = pricing.token_cost(&client_model, estimated_input, 0);
    let mut accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "embed",
            model: &client_model,
            provider: &chain[0].route.provider_name,
        },
        estimated_input,
        estimated_cost,
        pricing.clone(),
    )?;

    // Per-request cancellation. The guard fires on handler drop (client
    // disconnect), cancelling the token and aborting the in-flight upstream
    // call so the provider stops work.
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();

    // M9: resolve any remote image URLs to inline `data:` URIs under the
    // guarded-fetch policy, BEFORE batching/translation, so providers only ever
    // see inline bytes. A no-op for text and for `data:` URIs; honors
    // cancellation. Runs after admission so a rejected key never triggers a
    // fetch.
    let mut req = req;
    lumen_providers::image_fetch::resolve_image_parts(&mut req.input, &state.image_fetch, &cancel)
        .await?;

    // Execute across the chain with retries, fallback, circuit breaking and the
    // per-model timeouts. A fresh request clone (per attempt/link) carries that
    // link's upstream id.
    let executed =
        lumen_router::executor::execute(&links, &state.resilience.breakers, &exec, &cancel, |i| {
            let provider = chain[i].route.provider.clone();
            let cancel = cancel.clone();
            let mut attempt_req = req.clone();
            chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            async move {
                batch::embed_batched(
                    provider.as_ref(),
                    attempt_req,
                    &cancel,
                    batch::DEFAULT_CONCURRENCY,
                )
                .await
            }
        })
        .await?;
    // (an early return above drops `accounting`, refunding the reservation)

    let mut response = executed.value;
    accounting.served_by(&executed.model_used, &executed.provider_used);

    // ADR 003: upstream usage when reported, else the local estimate - never
    // a silent zero (e.g. TEI reports nothing). The response envelope always
    // carries the inline heuristic (never a BPE pass, never a wait).
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
    finish_embed(
        &state,
        accounting,
        &pricing,
        &executed.model_used,
        &req,
        tokens_in,
        estimated,
    );

    // Honor the client's requested output encoding (OpenAI `encoding_format`).
    // Providers always decode to `Vec<f32>` internally; re-encode to base64 on
    // the way out when asked, so the choice works for EVERY provider (Ollama and
    // TEI have no upstream encoding_format). Any other value serializes as the
    // default float array.
    if req.encoding_format.as_deref() == Some("base64") {
        for item in &mut response.data {
            item.encoding = lumen_core::EmbeddingEncoding::Base64;
        }
    }

    Ok((model_used_headers(&executed.model_used), Json(response)).into_response())
}

/// Close the embed accounting record: inline for upstream-reported or
/// heuristic counts; deferred to a background refinement task when the opt-in
/// accurate tokenizer refines this model (ADR 003). The deferred task recounts
/// the batch with exact BPE on the blocking pool, so usage_log and Prometheus
/// get the accurate number while the response (heuristic, flagged) has already
/// been returned. Latency is frozen first so the deferral never inflates it.
fn finish_embed(
    state: &AppState,
    mut accounting: Accounting,
    pricing: &std::sync::Arc<crate::pricing::CostTable>,
    model_used: &str,
    req: &lumen_core::EmbedRequest,
    tokens_in: u64,
    estimated: bool,
) {
    // M9: media accounting. `req.input`'s image parts are now `data:` URIs
    // (resolved before execution), so this measures decoded bytes with no I/O.
    let media = lumen_core::measure_media(&req.input);

    if estimated && state.token_counter.refines(model_used) {
        accounting.mark_completed();
        let counter = std::sync::Arc::clone(&state.token_counter);
        let model = model_used.to_owned();
        let texts: Vec<String> = req.input.iter().map(str::to_owned).collect();
        let pricing_task = pricing.clone();
        tokio::spawn(async move {
            let refined = counter
                .refine_embed(&model, texts)
                .await
                .unwrap_or(tokens_in);
            let cost = pricing_task.token_cost(&model, refined, 0);
            accounting.finish(&Outcome {
                tokens_in: refined,
                tokens_out: 0,
                estimated: true,
                search_units: None,
                breakdown: TokenBreakdown::default(),
                media,
                cost,
                status: 200,
            });
        });
    } else {
        let cost = pricing.token_cost(model_used, tokens_in, 0);
        accounting.finish(&Outcome {
            tokens_in,
            tokens_out: 0,
            estimated,
            search_units: None,
            breakdown: TokenBreakdown::default(),
            media,
            cost,
            status: 200,
        });
    }
}
