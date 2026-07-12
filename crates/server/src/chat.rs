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
//! envelope; a mid-stream error is emitted as a terminal SSE error frame. The
//! stream always ends with `data: [DONE]`.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferrogate_core::{ChatChunk, ChatRequest, GatewayError, ProviderError};
use futures::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::error::ApiError;
use crate::state::AppState;

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

    // Per-request cancellation. The guard fires on drop (client disconnect).
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    if req.stream {
        let stream = route
            .provider
            .chat_stream(req, cancel)
            .await
            .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        let body = to_sse_body(stream, provider_name, guard);
        Ok(Sse::new(body)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("ping"),
            )
            .into_response())
    } else {
        // Held across the await so a disconnect during the call cancels it.
        let _guard = guard;
        let response = route
            .provider
            .chat(req, cancel)
            .await
            .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        Ok(Json(response).into_response())
    }
}

/// Turn a provider chunk stream into an SSE body: each chunk becomes a
/// `data: {json}` frame, a mid-stream error becomes a terminal error frame, and
/// the stream always ends with `data: [DONE]`.
///
/// `guard` is moved into the mapping closure so it lives exactly as long as the
/// body: when the client disconnects, the body is dropped, the guard drops, and
/// the upstream call is cancelled.
fn to_sse_body(
    stream: BoxStream<'static, Result<ChatChunk, ProviderError>>,
    provider_name: String,
    guard: DropGuard,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    let frames = stream.map(move |item| {
        // Capture the guard so it stays alive for the whole stream.
        let _keepalive = &guard;
        let event = match item {
            Ok(chunk) => Event::default().data(serialize(&chunk)),
            Err(e) => {
                let ge = GatewayError::from_provider(&provider_name, e);
                Event::default().data(serialize(&ge.to_envelope()))
            }
        };
        Ok::<Event, Infallible>(event)
    });

    let done = stream::once(async { Ok(Event::default().data("[DONE]")) });
    frames.chain(done)
}

/// Serialize a value to a compact JSON string for an SSE `data:` field. A
/// serialization failure (should not happen for these types) degrades to an
/// empty object rather than panicking.
fn serialize<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned())
}
