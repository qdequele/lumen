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
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use ferrogate_core::{ChatRequest, GatewayError, ProviderError};
use futures::stream::{BoxStream, StreamExt};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::error::ApiError;
use crate::state::{AppState, StreamGuards};

/// Handle a chat completion request (streaming or not, per `stream`).
pub async fn chat(
    State(state): State<AppState>,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Malformed body → FG-1001 in our envelope (not axum's plain-text default).
    let Json(mut req) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    if req.messages.is_empty() {
        return Err(GatewayError::InvalidRequest("`messages` must not be empty".to_owned()).into());
    }

    // Resolve the client-facing model id to a provider + upstream id.
    let route = ferrogate_router::resolve_chat(&state.registry, &req.model)?;
    req.model = route.upstream_id.clone();
    let provider_name = route.provider_name.clone();
    let guards = state.guards;

    // Per-request cancellation. The guard fires on drop (client disconnect).
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    if req.stream {
        // Zero-copy passthrough where the upstream speaks OpenAI SSE; typed
        // providers translate event by event (ADR 004). Errors before the
        // first frame surface here as a JSON error envelope; if the upstream
        // never answers the request at all, the first-token timeout turns
        // into a plain 504 (headers are not sent yet at this point).
        let opened = tokio::time::timeout(
            guards.first_token_timeout,
            route.provider.chat_stream_bytes(req, cancel),
        )
        .await
        .map_err(|_| GatewayError::UpstreamFirstTokenTimeout {
            provider: provider_name.clone(),
        })?;
        let byte_stream = opened.map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        let body = Body::from_stream(to_event_stream(byte_stream, provider_name, guard, guards));
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
        let response =
            tokio::time::timeout(guards.first_token_timeout, route.provider.chat(req, cancel))
                .await
                .map_err(|_| GatewayError::UpstreamFirstTokenTimeout {
                    provider: provider_name.clone(),
                })?
                .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        Ok(Json(response).into_response())
    }
}

/// The `data: [DONE]` marker scanned for in forwarded frames.
const DONE_MARKER: &[u8] = b"data: [DONE]";

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
}

impl EventStreamState {
    /// Record a forwarded frame, detecting `data: [DONE]` even when the marker
    /// is split across frame boundaries.
    fn scan_frame(&mut self, frame: &[u8]) {
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(frame);
        if window.windows(DONE_MARKER.len()).any(|w| w == DONE_MARKER) {
            self.saw_done = true;
        }
        let keep = window.len().min(DONE_MARKER.len() - 1);
        self.tail = window.split_off(window.len() - keep);
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
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
    let state = EventStreamState {
        inner: stream,
        provider: provider_name,
        _cancel_on_drop: guard,
        guards,
        first_frame_deadline: tokio::time::Instant::now() + guards.first_token_timeout,
        got_first_frame: false,
        saw_done: false,
        ended: false,
        tail: Vec::new(),
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
                error_frame(&GatewayError::from_provider(&s.provider, e))
            }
            Step::Item(None) => {
                s.ended = true;
                if s.saw_done {
                    // Clean upstream termination — nothing left to add.
                    return None;
                }
                // The upstream died without its terminator (criterion 5).
                error_frame(&GatewayError::UpstreamStreamInterrupted {
                    provider: s.provider.clone(),
                })
            }
            Step::Ping => Bytes::from_static(b": ping\n\n"),
            Step::FirstTokenTimeout => {
                s.ended = true;
                error_frame(&GatewayError::UpstreamFirstTokenTimeout {
                    provider: s.provider.clone(),
                })
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
        let out = collect(to_event_stream(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\n")),
                Ok(Bytes::from_static(b"data: [DONE]\n\n")),
            ]),
            "p".to_owned(),
            drop_guard(),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out, vec!["data: {\"x\":1}\n\n", "data: [DONE]\n\n"]);
    }

    #[tokio::test]
    async fn missing_done_terminator_appends_fg_3010_error_frame() {
        let out = collect(to_event_stream(
            frames(vec![Ok(Bytes::from_static(b"data: {\"x\":1}\n\n"))]),
            "p".to_owned(),
            drop_guard(),
            guards(30_000, 15_000),
        ))
        .await;
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("FG-3010"), "got: {}", out[1]);
        assert!(out[1].contains("upstream_error"));
    }

    #[tokio::test]
    async fn done_marker_split_across_frames_is_still_detected() {
        let out = collect(to_event_stream(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\ndata: [DO")),
                Ok(Bytes::from_static(b"NE]\n\n")),
            ]),
            "p".to_owned(),
            drop_guard(),
            guards(30_000, 15_000),
        ))
        .await;
        // No FG-3010: the split terminator was recognised.
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|f| f.contains("FG-3010")));
    }

    #[tokio::test]
    async fn mid_stream_provider_error_becomes_terminal_error_frame() {
        let out = collect(to_event_stream(
            frames(vec![
                Ok(Bytes::from_static(b"data: {\"x\":1}\n\n")),
                Err(ProviderError::Unavailable {
                    provider: "p".to_owned(),
                }),
                // Anything after the error must not be forwarded.
                Ok(Bytes::from_static(b"data: {\"x\":2}\n\n")),
            ]),
            "p".to_owned(),
            drop_guard(),
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
        let out = collect(to_event_stream(
            stream::pending().boxed(),
            "p".to_owned(),
            drop_guard(),
            guards(40, 15),
        ))
        .await;
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
        let wrapped = to_event_stream(idle, "p".to_owned(), drop_guard(), guards(50, 15));
        let out: Vec<String> = collect(wrapped.take(4)).await;
        assert_eq!(out[0], "data: {\"x\":1}\n\n");
        assert!(out[1..].iter().all(|f| f == ": ping\n\n"), "got: {out:?}");
    }
}
