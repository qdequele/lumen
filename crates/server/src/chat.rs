//! `POST /v1/chat/completions` — non-streaming JSON and streaming SSE.
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
//! * **first-token timeout** — no first frame within the configured window →
//!   FG-3011 error frame (or a plain 504 when the upstream never even answered
//!   the request);
//! * **missing terminator** — the upstream ends the stream without ever sending
//!   `data: [DONE]` → FG-3010 error frame (the terminator only ever comes from
//!   the provider byte stream: verbatim upstream for passthrough, translated
//!   terminal event otherwise — the server never fabricates it);
//! * **heartbeat** — a `: ping` SSE comment on idle streams so intermediaries
//!   don't reap a slow upstream.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bytes::Bytes;
use ferrogate_core::{tokens, ChatRequest, GatewayError, ProviderError, Usage};
use futures::stream::{BoxStream, StreamExt};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::accounting::{Accounting, Outcome, StreamAccounting, Target};
use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::pricing::DEFAULT_RESERVED_OUTPUT_TOKENS;
use crate::state::{AppState, StreamGuards};

/// Handle a chat completion request (streaming or not, per `stream`).
pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Option<Extension<AuthedKey>>,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → FG-1001 in our envelope (not axum's plain-text default).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.messages.is_empty() {
        return Err(GatewayError::InvalidRequest("`messages` must not be empty".to_owned()).into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let client_model = req.model.clone();
    let route = ferrogate_router::resolve_chat(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();
    let provider_name = route.provider_name.clone();
    let guards = state.guards;

    // Admission BEFORE the upstream call (M5 §5.2): reserve the pre-call
    // estimate (prompt heuristic + `max_tokens`, or a default output
    // reservation) against the key's budget; TPM counts the same estimate.
    // The reservation is settled to the real usage afterwards.
    let estimated_input = tokens::estimate_chat_prompt(&req);
    let reserved_output = req
        .max_tokens
        .map_or(DEFAULT_RESERVED_OUTPUT_TOKENS, u64::from);
    let estimated_cost = state
        .pricing
        .token_cost(&client_model, estimated_input, reserved_output);
    let accounting = Accounting::begin(
        &state,
        &headers,
        key.as_deref(),
        Target {
            capability: "chat",
            model: &client_model,
            provider: &route.provider_name,
        },
        estimated_input + reserved_output,
        estimated_cost,
    )?;

    // Per-request cancellation. The guard fires on drop (client disconnect).
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    if req.stream {
        // Zero-copy passthrough where the upstream speaks OpenAI SSE; typed
        // providers translate event by event (ADR 004). Errors before the
        // first frame surface here as a JSON error envelope; if the upstream
        // never answers the request at all, the first-token timeout turns
        // into a plain 504 (headers are not sent yet at this point). ONE
        // absolute deadline covers both phases (response headers, then first
        // SSE frame) so the configured window is a true end-to-end bound.
        let deadline = tokio::time::Instant::now() + guards.first_token_timeout;
        let opened =
            tokio::time::timeout_at(deadline, route.provider.chat_stream_bytes(req, cancel))
                .await
                .map_err(|_| GatewayError::UpstreamFirstTokenTimeout {
                    provider: provider_name.clone(),
                })?;
        let byte_stream = opened.map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        // Accounting rides inside the stream: the sniffer reads the final
        // usage chunk (or falls back to estimates) and settles when the
        // stream ends — including on client disconnect, via its Drop.
        let stream_accounting = StreamAccounting::new(accounting, estimated_input);
        let body = Body::from_stream(to_event_stream(
            byte_stream,
            provider_name,
            guard,
            guards,
            deadline,
            Some(stream_accounting),
        ));
        Ok((
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response())
    } else {
        // Held across the await so a disconnect during the call cancels it.
        let _guard = guard;
        // Non-streaming has no observable "first token": the whole upstream
        // call gets the window (per-phase refinement lands in M6).
        let mut response =
            tokio::time::timeout(guards.first_token_timeout, route.provider.chat(req, cancel))
                .await
                .map_err(|_| GatewayError::UpstreamFirstTokenTimeout {
                    provider: provider_name.clone(),
                })?
                .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        // (an early return above drops `accounting`, refunding the reservation)
        settle_non_streaming(
            accounting,
            &state,
            &client_model,
            estimated_input,
            &mut response,
        );
        Ok(Json(response).into_response())
    }
}

/// Close the books on a non-streaming completion (ADR 003): upstream usage
/// when reported, else local estimates — never a silent zero. The estimate is
/// surfaced (flagged) in the response body too.
fn settle_non_streaming(
    accounting: Accounting,
    state: &AppState,
    client_model: &str,
    estimated_input: u64,
    response: &mut ferrogate_core::ChatResponse,
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
                    .as_deref()
                    .map_or(0, tokens::estimate_text)
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
    let cost = state
        .pricing
        .token_cost(client_model, tokens_in, tokens_out);
    accounting.finish(&Outcome {
        tokens_in,
        tokens_out,
        estimated,
        search_units: None,
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
    /// Set after a terminal frame (error / FG-3010 / FG-3011): stream is over.
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
    /// for client disconnects (which finalize as 200 — the status the
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
/// by the translator on a genuine upstream terminal event — ADR 004), so the
/// server does not re-frame and never fabricates the terminator. On top of
/// verbatim forwarding this wrapper adds the three guards described in the
/// module docs (FG-3011 first-token timeout, FG-3010 missing terminator,
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
                    // Clean upstream termination — nothing left to add.
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
        assert!(out[1].contains("FG-3010"), "got: {}", out[1]);
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
        // No FG-3010: the split terminator was recognised.
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|f| f.contains("FG-3010")));
    }

    #[tokio::test]
    async fn done_marker_inside_model_content_does_not_suppress_fg_3010() {
        // The MODEL's own text contains "data: [DONE]" (inside a JSON string,
        // mid-line). Only a line-anchored terminator counts: when the upstream
        // then dies without a real [DONE], FG-3010 must still fire.
        let out = collect(wrap(
            frames(vec![Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"data: [DONE]\"}}]}\n\n",
            ))]),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out.len(), 2, "got: {out:?}");
        assert!(out[1].contains("FG-3010"), "got: {}", out[1]);
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
        assert!(out[1].contains("FG-3003") || out[1].contains("upstream_error"));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_upstream_gets_heartbeat_pings_then_first_token_timeout() {
        // First-token window of 40 ms with a 15 ms heartbeat: two pings
        // (15, 30), then FG-3011 at 40. Paused time makes this exact.
        let out = collect(wrap(stream::pending().boxed(), guards(40, 15))).await;
        assert_eq!(out.len(), 3, "got: {out:?}");
        assert_eq!(out[0], ": ping\n\n");
        assert_eq!(out[1], ": ping\n\n");
        assert!(out[2].contains("FG-3011"));
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
