//! `POST /v1/chat/completions` - non-streaming JSON and streaming SSE.
//!
//! Flow: validate → route (model → provider) → chat / chat_stream. A per-request
//! [`CancellationToken`] is cancelled when the client disconnects, aborting the
//! upstream call:
//!
//! * non-streaming: the drop guard is held across the `chat` await;
//! * streaming: the guard is moved INTO the SSE body stream, so dropping the
//!   response (client disconnect) cancels the token and aborts the upstream.
//!
//! Errors that occur before streaming starts are returned as a normal JSON error
//! envelope; a mid-stream error is emitted as a terminal SSE error frame.
//!
//! The streaming body is wrapped in three guards (see [`to_event_stream`]):
//!
//! * **first-token timeout** - no first frame within the configured window →
//!   LM-3011 error frame (or a plain 504 when the upstream never even answered
//!   the request);
//! * **missing terminator** - the upstream ends the stream without ever sending
//!   `data: [DONE]` → LM-3010 error frame (the terminator only ever comes from
//!   the provider byte stream: verbatim upstream for passthrough, translated
//!   terminal event otherwise - the server never fabricates it);
//! * **heartbeat** - a `: ping` SSE comment on idle streams so intermediaries
//!   don't reap a slow upstream.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use lumen_core::{tokens, ChatRequest, GatewayError, ProviderError, Usage};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::accounting::{Accounting, Outcome, StreamAccounting, Target};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::pricing::DEFAULT_RESERVED_OUTPUT_TOKENS;
use crate::resilience::model_used_headers;
use crate::state::{AppState, StreamGuards};

/// Handle a chat completion request (streaming or not, per `stream`).
///
/// Both modes run through the M6 resilience executor: the requested model plus
/// its configured fallbacks are tried in turn with retries, circuit breaking
/// and the per-model timeouts (ADR 005). For streaming, retry/fallback happen
/// only while *opening* the upstream byte stream - once the first frame is
/// forwarded the request is committed and the M4 frame guards take over.
pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → LM-1001 in our envelope (not axum's plain-text default).
    //
    // An over-limit body never reaches here: `RequestBodyLimitLayer` (see
    // `app.rs`) short-circuits at the tower layer with a bare 413 before axum's
    // routing/extraction runs, so `JsonRejection` never surfaces
    // `PAYLOAD_TOO_LARGE` - verified empirically (a debug probe in this
    // `map_err` never fired for an over-limit request). `app::map_body_limit_response`
    // rewrites that bare 413 into the `LM-1002` envelope instead.
    let Json(req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.messages.is_empty() {
        return Err(GatewayError::InvalidRequest("`messages` must not be empty".to_owned()).into());
    }

    // Resolve the requested model to a fallback chain (M6 §6.2).
    let client_model = req.model.clone();
    let chain_ids = state.resilience.chain_ids(&client_model);
    let chain = lumen_router::resolve_chat_chain(&state.registry, &chain_ids)?;
    enforce_image_support(&state, &client_model, &chain, &req)?;
    let links = lumen_router::chat_links(&chain);
    let exec = state.resilience.exec_config(&client_model);

    // Admission BEFORE the upstream call (M5 §5.2): reserve the pre-call
    // estimate (prompt heuristic + `max_tokens`, or a default output
    // reservation) against the key's budget; TPM counts the same estimate.
    // The reservation is settled to the real usage afterwards.
    let pricing = state.pricing();
    let estimated_input = tokens::estimate_chat_prompt(&req);
    let reserved_output = req
        .max_tokens
        .map_or(DEFAULT_RESERVED_OUTPUT_TOKENS, u64::from);
    let estimated_cost = pricing.token_cost(&client_model, estimated_input, reserved_output);
    let accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "chat",
            model: &client_model,
            provider: &chain[0].route.provider_name,
        },
        estimated_input + reserved_output,
        estimated_cost,
        pricing,
    )?;

    // Per-request cancellation. The guard fires on drop (client disconnect).
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    let ctx = ChatExec {
        state: &state,
        chain: &chain,
        links: &links,
        exec,
        cancel: &cancel,
        req: &req,
        estimated_input,
    };
    if req.stream {
        chat_streaming(&ctx, guard, accounting).await
    } else {
        chat_non_streaming(&ctx, guard, accounting).await
    }
}

/// Reject image inputs the resolved route cannot serve, before any upstream
/// call: `LM-2003` if the model is not declared vision-capable, `LM-2004` if a
/// remote image URL is bound for a provider that only takes inline base64.
fn enforce_image_support(
    state: &AppState,
    client_model: &str,
    chain: &[lumen_router::ChatChainLink],
    req: &ChatRequest,
) -> Result<(), GatewayError> {
    let has_image = req.messages.iter().any(|m| {
        m.content
            .as_ref()
            .is_some_and(lumen_core::MessageContent::has_image)
    });
    if !has_image {
        return Ok(());
    }
    // LM-2003: model must declare the "image" modality.
    let vision_ok = state
        .registry
        .modalities(client_model)
        .is_some_and(|mods| mods.iter().any(|m| m == "image"));
    if !vision_ok {
        return Err(GatewayError::ImageInputNotSupported {
            model: client_model.to_owned(),
        });
    }
    // LM-2004: if the PRIMARY provider can't take a remote URL, reject one.
    let has_remote_url = req.messages.iter().any(|m| {
        matches!(m.content.as_ref(), Some(lumen_core::MessageContent::Parts(parts))
            if parts.iter().any(|p| p.image_url.as_ref().is_some_and(lumen_core::ImageUrl::is_remote)))
    });
    if has_remote_url && !chain[0].route.provider.accepts_remote_image_url() {
        return Err(GatewayError::ImageUrlNotSupported {
            provider: chain[0].route.provider_name.clone(),
        });
    }
    Ok(())
}

/// Everything the two chat execution paths share, bundled so the helpers stay
/// under the argument-count lint.
struct ChatExec<'a> {
    state: &'a AppState,
    chain: &'a [lumen_router::ChatChainLink],
    links: &'a [lumen_router::executor::Link],
    exec: lumen_router::executor::ExecConfig,
    cancel: &'a CancellationToken,
    req: &'a ChatRequest,
    estimated_input: u64,
}

/// Streaming path: open the upstream byte stream with retry/fallback (only
/// before the first frame, spec 6.2), then hand it to the M4 frame guards.
/// Zero-copy passthrough where the upstream speaks OpenAI SSE; typed providers
/// translate event by event (ADR 004). An open failure surfaces as a JSON error
/// envelope (headers not sent yet).
async fn chat_streaming(
    ctx: &ChatExec<'_>,
    guard: DropGuard,
    mut accounting: Accounting,
) -> Result<Response, ApiError> {
    let executed = lumen_router::executor::execute(
        ctx.links,
        &ctx.state.resilience.breakers,
        &ctx.exec,
        ctx.cancel,
        |i| {
            let provider = ctx.chain[i].route.provider.clone();
            let cancel = ctx.cancel.clone();
            let mut attempt_req = ctx.req.clone();
            attempt_req.stream = true;
            ctx.chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            async move { provider.chat_stream_bytes(attempt_req, cancel).await }
        },
    )
    .await?;
    // (an early return above drops `accounting`, refunding the reservation)
    accounting.served_by(&executed.model_used, &executed.provider_used);

    // A fresh first-frame deadline now that the stream is open (the open phase
    // already had its own first-token budget). The M4 frame guards
    // (LM-3010/3011, heartbeat) own the stream from here - no more retries.
    let deadline = tokio::time::Instant::now() + ctx.exec.first_token;
    let stream_accounting = StreamAccounting::new(accounting, ctx.estimated_input);
    let body = Body::from_stream(to_event_stream(
        executed.value,
        executed.provider_used.clone(),
        guard,
        ctx.state.guards,
        deadline,
        Some(stream_accounting),
    ));
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        model_used_headers(&executed.model_used),
        body,
    )
        .into_response())
}

/// Non-streaming path: one JSON completion across the chain, settled inline.
async fn chat_non_streaming(
    ctx: &ChatExec<'_>,
    guard: DropGuard,
    mut accounting: Accounting,
) -> Result<Response, ApiError> {
    // Held across the await so a disconnect during the call cancels it.
    let _guard = guard;
    let executed = lumen_router::executor::execute(
        ctx.links,
        &ctx.state.resilience.breakers,
        &ctx.exec,
        ctx.cancel,
        |i| {
            let provider = ctx.chain[i].route.provider.clone();
            let cancel = ctx.cancel.clone();
            let mut attempt_req = ctx.req.clone();
            ctx.chain[i]
                .route
                .upstream_id
                .clone_into(&mut attempt_req.model);
            async move { provider.chat(attempt_req, cancel).await }
        },
    )
    .await?;
    // (an early return above drops `accounting`, refunding the reservation)
    accounting.served_by(&executed.model_used, &executed.provider_used);
    let mut response = executed.value;
    let served_model = executed.model_used.clone();
    settle_non_streaming(
        accounting,
        &served_model,
        ctx.estimated_input,
        &mut response,
    );
    Ok((model_used_headers(&served_model), Json(response)).into_response())
}

/// Close the books on a non-streaming completion (ADR 003): upstream usage
/// when reported, else local estimates - never a silent zero. The estimate is
/// surfaced (flagged) in the response body too.
fn settle_non_streaming(
    accounting: Accounting,
    client_model: &str,
    estimated_input: u64,
    response: &mut lumen_core::ChatResponse,
) {
    let reported = response
        .usage
        .filter(|u| u.prompt_tokens > 0 || u.completion_tokens > 0);
    let (tokens_in, tokens_out, estimated) = if let Some(usage) = reported {
        (
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
            false,
        )
    } else {
        let output: u64 = response
            .choices
            .iter()
            .map(|c| {
                c.message
                    .content
                    .as_ref()
                    .map_or(0, |c| tokens::estimate_text(&c.text()))
            })
            .sum();
        (estimated_input, output, true)
    };
    if estimated {
        response.usage = Some(Usage {
            prompt_tokens: u32::try_from(tokens_in).unwrap_or(u32::MAX),
            completion_tokens: u32::try_from(tokens_out).unwrap_or(u32::MAX),
            total_tokens: u32::try_from(tokens_in + tokens_out).unwrap_or(u32::MAX),
            estimated: Some(true),
        });
    }
    let cost = accounting
        .pricing()
        .token_cost(client_model, tokens_in, tokens_out);
    accounting.finish(&Outcome {
        tokens_in,
        tokens_out,
        estimated,
        search_units: None,
        media: lumen_core::MediaUsage::default(),
        cost,
        status: 200,
    });
}

/// The `data: [DONE]` marker scanned for in forwarded frames. Anchored on a
/// line start (`\n`): a raw `0x0A` cannot appear inside a JSON string (serde
/// escapes it as the two characters `\n`), so model content that literally
/// says "data: [DONE]" inside a delta can never spoof the terminator.
const DONE_MARKER: &[u8] = b"\ndata: [DONE]";

/// One SSE frame carrying the standard JSON error envelope.
fn error_frame(error: &GatewayError) -> Bytes {
    let json = serde_json::to_string(&error.to_envelope()).unwrap_or_else(|_| "{}".to_owned());
    Bytes::from(format!("data: {json}\n\n"))
}

/// What the guarded wait for the next upstream frame produced.
enum Step {
    /// The inner stream yielded (frame, error, or end).
    Item(Option<Result<Bytes, ProviderError>>),
    /// The heartbeat interval elapsed with no upstream traffic.
    Ping,
    /// The first frame never arrived within the first-token window.
    FirstTokenTimeout,
}

/// Streaming wrapper state. Bounded: no frame content is retained beyond the
/// few bytes needed to detect a `[DONE]` split across frame boundaries.
struct EventStreamState {
    inner: BoxStream<'static, Result<Bytes, ProviderError>>,
    provider: String,
    /// Client disconnect drops the body → drops this → aborts the upstream.
    _cancel_on_drop: DropGuard,
    guards: StreamGuards,
    /// Absolute deadline for the first upstream frame.
    first_frame_deadline: tokio::time::Instant,
    got_first_frame: bool,
    /// Whether a terminal `data: [DONE]` was seen in the forwarded bytes.
    saw_done: bool,
    /// Set after a terminal frame (error / LM-3010 / LM-3011): stream is over.
    ended: bool,
    /// Trailing bytes of the previous frame, so a `[DONE]` marker split across
    /// two frames is still detected. At most `DONE_MARKER.len() - 1` bytes.
    tail: Vec<u8>,
    /// Token/cost accounting riding along the stream (M5). `None` in unit
    /// tests that only exercise the guards. Its `Drop` settles the books, so
    /// a client disconnect can never leak a budget reservation.
    accounting: Option<StreamAccounting>,
}

impl EventStreamState {
    /// Record a forwarded frame, detecting `data: [DONE]` even when the marker
    /// is split across frame boundaries.
    fn scan_frame(&mut self, frame: &[u8]) {
        if let Some(accounting) = &mut self.accounting {
            accounting.scan(frame);
        }
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(frame);
        if window.windows(DONE_MARKER.len()).any(|w| w == DONE_MARKER) {
            self.saw_done = true;
        }
        let keep = window.len().min(DONE_MARKER.len() - 1);
        self.tail = window.split_off(window.len() - keep);
    }

    /// Close the accounting record with the stream's terminal outcome, so
    /// `usage_log.status` reflects what actually happened (200 = clean end,
    /// 502/504 = in-band error frame). Idempotent; Drop is the safety net
    /// for client disconnects (which finalize as 200 - the status the
    /// client's response actually carried).
    fn settle_accounting(&mut self, status: u16) {
        if let Some(mut accounting) = self.accounting.take() {
            accounting.finalize_with_status(status);
        }
    }
}

/// Forward a provider's raw SSE `Bytes` stream into the response body, guarded.
///
/// The provider's byte stream carries complete SSE framing and its own terminal
/// `data: [DONE]\n\n` (verbatim from the upstream for passthrough, or emitted
/// by the translator on a genuine upstream terminal event - ADR 004), so the
/// server does not re-frame and never fabricates the terminator. On top of
/// verbatim forwarding this wrapper adds the three guards described in the
/// module docs (LM-3011 first-token timeout, LM-3010 missing terminator,
/// `: ping` heartbeat). A mid-stream provider error becomes a terminal SSE
/// error frame carrying the standard JSON envelope.
///
/// `guard` is moved into the state so it lives exactly as long as the body: a
/// client disconnect drops the body, drops the guard, and aborts the upstream
/// byte stream.
fn to_event_stream(
    stream: BoxStream<'static, Result<Bytes, ProviderError>>,
    provider_name: String,
    guard: DropGuard,
    guards: StreamGuards,
    first_frame_deadline: tokio::time::Instant,
    accounting: Option<StreamAccounting>,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
    let state = EventStreamState {
        inner: stream,
        provider: provider_name,
        _cancel_on_drop: guard,
        guards,
        first_frame_deadline,
        got_first_frame: false,
        saw_done: false,
        ended: false,
        // Seeded with a virtual line start so a `[DONE]` opening the very
        // first frame still matches the line-anchored marker.
        tail: vec![b'\n'],
        accounting,
    };

    futures::stream::unfold(state, |mut s| async move {
        if s.ended {
            return None;
        }

        let step = {
            let heartbeat = tokio::time::sleep(s.guards.heartbeat_interval);
            tokio::pin!(heartbeat);
            if s.got_first_frame {
                tokio::select! {
                    biased;
                    item = s.inner.next() => Step::Item(item),
                    () = &mut heartbeat => Step::Ping,
                }
            } else {
                tokio::select! {
                    biased;
                    item = s.inner.next() => Step::Item(item),
                    // On a tie, the harder deadline wins over a mere ping.
                    () = tokio::time::sleep_until(s.first_frame_deadline) => {
                        Step::FirstTokenTimeout
                    }
                    () = &mut heartbeat => Step::Ping,
                }
            }
        };

        let frame = match step {
            Step::Item(Some(Ok(frame))) => {
                s.got_first_frame = true;
                s.scan_frame(&frame);
                frame
            }
            Step::Item(Some(Err(e))) => {
                // Mid-stream provider error: terminal error frame, then done.
                s.ended = true;
                let error = GatewayError::from_provider(&s.provider, e);
                s.settle_accounting(error.http_status());
                error_frame(&error)
            }
            Step::Item(None) => {
                s.ended = true;
                if s.saw_done {
                    // Clean upstream termination - nothing left to add.
                    s.settle_accounting(200);
                    return None;
                }
                // The upstream died without its terminator (criterion 5).
                let error = GatewayError::UpstreamStreamInterrupted {
                    provider: s.provider.clone(),
                };
                s.settle_accounting(error.http_status());
                error_frame(&error)
            }
            Step::Ping => Bytes::from_static(b": ping\n\n"),
            Step::FirstTokenTimeout => {
                s.ended = true;
                let error = GatewayError::UpstreamFirstTokenTimeout {
                    provider: s.provider.clone(),
                };
                s.settle_accounting(error.http_status());
                error_frame(&error)
            }
        };
        Some((Ok::<Bytes, Infallible>(frame), s))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::time::Duration;

    fn guards(first_token_ms: u64, heartbeat_ms: u64) -> StreamGuards {
        StreamGuards {
            first_token_timeout: Duration::from_millis(first_token_ms),
            heartbeat_interval: Duration::from_millis(heartbeat_ms),
        }
    }

    fn drop_guard() -> DropGuard {
        CancellationToken::new().drop_guard()
    }

    /// Wrap a stream with guards, deadline = now + first_token_timeout.
    fn wrap(
        stream: BoxStream<'static, Result<Bytes, ProviderError>>,
        g: StreamGuards,
    ) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
        let deadline = tokio::time::Instant::now() + g.first_token_timeout;
        to_event_stream(stream, "p".to_owned(), drop_guard(), g, deadline, None)
    }

    fn frames(
        items: Vec<Result<Bytes, ProviderError>>,
    ) -> BoxStream<'static, Result<Bytes, ProviderError>> {
        stream::iter(items).boxed()
    }

    async fn collect(
        stream: impl futures::Stream<Item = Result<Bytes, Infallible>>,
    ) -> Vec<String> {
        stream
            .map(|item| {
                let Ok(bytes) = item;
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn forwards_frames_verbatim_and_adds_nothing_after_done() {
        let out = collect(wrap(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\n")),
                Ok(Bytes::from_static(b"data: [DONE]\n\n")),
            ]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out, vec!["data: {\"x\":1}\n\n", "data: [DONE]\n\n"]);
    }

    #[tokio::test]
    async fn missing_done_terminator_appends_fg_3010_error_frame() {
        let out = collect(wrap(
            frames(vec![Ok(Bytes::from_static(b"data: {\"x\":1}\n\n"))]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("LM-3010"), "got: {}", out[1]);
        assert!(out[1].contains("upstream_error"));
    }

    #[tokio::test]
    async fn done_marker_split_across_frames_is_still_detected() {
        let out = collect(wrap(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\ndata: [DO")),
                Ok(Bytes::from_static(b"NE]\n\n")),
            ]),
            guards(30_000, 15_000),
        ))
        .await;
        // No LM-3010: the split terminator was recognised.
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|f| f.contains("LM-3010")));
    }

    #[tokio::test]
    async fn done_marker_inside_model_content_does_not_suppress_fg_3010() {
        // The MODEL's own text contains "data: [DONE]" (inside a JSON string,
        // mid-line). Only a line-anchored terminator counts: when the upstream
        // then dies without a real [DONE], LM-3010 must still fire.
        let out = collect(wrap(
            frames(vec![Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"data: [DONE]\"}}]}\n\n",
            ))]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out.len(), 2, "got: {out:?}");
        assert!(out[1].contains("LM-3010"), "got: {}", out[1]);
    }

    #[tokio::test]
    async fn done_marker_as_the_very_first_frame_is_recognised() {
        // The seeded virtual line start must let a stream whose first bytes
        // are the terminator itself end cleanly.
        let out = collect(wrap(
            frames(vec![Ok(Bytes::from_static(b"data: [DONE]\n\n"))]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out, vec!["data: [DONE]\n\n"]);
    }

    #[tokio::test]
    async fn mid_stream_provider_error_becomes_terminal_error_frame() {
        let out = collect(wrap(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\n")),
                Err(ProviderError::Unavailable {
                    provider: "p".to_owned(),
                }),
                // Anything after the error must not be forwarded.
                Ok(Bytes::from_static(b"data: {\"x\":2}\n\n")),
            ]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("LM-3003") || out[1].contains("upstream_error"));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_upstream_gets_heartbeat_pings_then_first_token_timeout() {
        // First-token window of 40 ms with a 15 ms heartbeat: two pings
        // (15, 30), then LM-3011 at 40. Paused time makes this exact.
        let out = collect(wrap(stream::pending().boxed(), guards(40, 15))).await;
        assert_eq!(out.len(), 3, "got: {out:?}");
        assert_eq!(out[0], ": ping\n\n");
        assert_eq!(out[1], ": ping\n\n");
        assert!(out[2].contains("LM-3011"));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_pings_keep_an_idle_mid_stream_alive() {
        // One frame arrives, then the upstream stays silent forever: after the
        // first frame the first-token deadline no longer applies, so we get
        // pings indefinitely. Take a handful and stop.
        let idle = stream::iter(vec![Ok(Bytes::from_static(b"data: {\"x\":1}\n\n"))])
            .chain(stream::pending())
            .boxed();
        let wrapped = wrap(idle, guards(50, 15));
        let out: Vec<String> = collect(wrapped.take(4)).await;
        assert_eq!(out[0], "data: {\"x\":1}\n\n");
        assert!(out[1..].iter().all(|f| f == ": ping\n\n"), "got: {out:?}");
    }
}
