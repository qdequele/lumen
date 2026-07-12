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
//! envelope; a mid-stream error is emitted as a terminal SSE error frame. On a
//! successful stream the terminal `data: [DONE]` comes from the provider byte
//! stream itself (verbatim upstream for passthrough, or appended by the typed
//! default) — the server never adds or duplicates it.

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
        // Zero-copy passthrough where the upstream speaks OpenAI SSE; typed
        // providers fall back to the serializing default (see ADR 004). Errors
        // before the first frame surface here as a JSON error envelope.
        let byte_stream = route
            .provider
            .chat_stream_bytes(req, cancel)
            .await
            .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        let body = Body::from_stream(to_event_stream(byte_stream, provider_name, guard));
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
        let response = route
            .provider
            .chat(req, cancel)
            .await
            .map_err(|e| GatewayError::from_provider(&provider_name, e))?;
        Ok(Json(response).into_response())
    }
}

/// Forward a provider's raw SSE `Bytes` stream into the response body.
///
/// The provider's byte stream already carries complete SSE framing and its own
/// terminal `data: [DONE]\n\n` (verbatim from the upstream for passthrough, or
/// appended by the serializing default for typed providers — ADR 004), so the
/// server does not re-frame. A mid-stream provider error becomes a terminal SSE
/// error frame carrying the standard JSON envelope.
///
/// `guard` is moved into the mapping closure so it lives exactly as long as the
/// body: a client disconnect drops the body, drops the guard, and aborts the
/// upstream byte stream.
fn to_event_stream(
    stream: BoxStream<'static, Result<Bytes, ProviderError>>,
    provider_name: String,
    guard: DropGuard,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
    stream.map(move |item| {
        // Capture the guard so it stays alive for the whole stream.
        let _keepalive = &guard;
        let bytes = match item {
            Ok(frame) => frame,
            Err(e) => {
                let ge = GatewayError::from_provider(&provider_name, e);
                let json =
                    serde_json::to_string(&ge.to_envelope()).unwrap_or_else(|_| "{}".to_owned());
                Bytes::from(format!("data: {json}\n\n"))
            }
        };
        Ok::<Bytes, Infallible>(bytes)
    })
}
