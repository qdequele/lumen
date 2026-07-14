//! Shared chat helpers.
//!
//! * [`single_shot_stream`] adapts a complete [`ChatResponse`] into a one-frame
//!   [`ChatChunk`] stream (fallback used by providers without native streaming).
//! * [`SseTranslator`] + [`translate_sse_stream`] are the shared plumbing for
//!   providers whose streaming wire format is NOT OpenAI (Anthropic, Google):
//!   upstream bytes → incremental SSE parse → provider-specific event
//!   translation → OpenAI chunks. Translation state is bounded — the full
//!   response text is never accumulated.
//! * [`items_to_sse_bytes`] / [`items_to_chunks`] adapt the translated item
//!   stream to the two `ChatProvider` streaming signatures. The terminal
//!   `data: [DONE]` is emitted only when the translator saw a genuine upstream
//!   terminal event, so a mid-stream upstream death is detectable downstream
//!   (LM-3010 — the server appends the error frame).

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lumen_core::{ChatChunk, ChatChunkChoice, ChatDelta, ChatRequest, ChatResponse, ProviderError};

use crate::sse::{SseEvent, SseParser};

/// One item of a translated stream: an OpenAI chunk, or the upstream's genuine
/// terminal event (which becomes `data: [DONE]` on the byte path).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// A translated OpenAI `chat.completion.chunk`.
    Chunk(ChatChunk),
    /// The upstream signalled a clean end of stream.
    Done,
}

/// Provider-specific translation of upstream SSE events into OpenAI chunks.
///
/// Implementations must keep their state bounded: never accumulate the full
/// response content.
pub trait SseTranslator: Send + 'static {
    /// Translate one upstream event into zero or more stream items.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the event is malformed or the upstream
    /// signalled an in-stream error.
    fn translate(&mut self, event: &SseEvent) -> Result<Vec<StreamItem>, ProviderError>;
}

/// Pipe an upstream byte stream through the incremental SSE parser and a
/// provider translator, yielding translated [`StreamItem`]s.
pub fn translate_sse_stream<T: SseTranslator>(
    bytes: BoxStream<'static, Result<Bytes, ProviderError>>,
    translator: T,
) -> BoxStream<'static, Result<StreamItem, ProviderError>> {
    bytes
        .scan(
            (SseParser::new(), translator),
            |(parser, translator), item| {
                let out: Vec<Result<StreamItem, ProviderError>> = match item {
                    Ok(chunk) => match parser.push(&chunk) {
                        Ok(events) => events
                            .iter()
                            .flat_map(|event| match translator.translate(event) {
                                Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
                                Err(e) => vec![Err(e)],
                            })
                            .collect(),
                        Err(e) => vec![Err(e)],
                    },
                    Err(e) => vec![Err(e)],
                };
                futures::future::ready(Some(stream::iter(out)))
            },
        )
        .flatten()
        .boxed()
}

/// Adapt a translated item stream to the `chat_stream_bytes` signature:
/// each chunk becomes a `data: {json}\n\n` frame; the upstream's terminal event
/// becomes `data: [DONE]\n\n`. No `[DONE]` is fabricated — if the upstream dies
/// mid-stream the byte stream simply ends, which the server turns into LM-3010.
pub fn items_to_sse_bytes(
    items: BoxStream<'static, Result<StreamItem, ProviderError>>,
) -> BoxStream<'static, Result<Bytes, ProviderError>> {
    items
        .map(|item| {
            item.map(|item| match item {
                StreamItem::Chunk(chunk) => {
                    let json = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".to_owned());
                    Bytes::from(format!("data: {json}\n\n"))
                }
                StreamItem::Done => Bytes::from_static(b"data: [DONE]\n\n"),
            })
        })
        .boxed()
}

/// Adapt a translated item stream to the typed `chat_stream` signature
/// (the terminal marker is dropped; the stream just ends).
pub fn items_to_chunks(
    items: BoxStream<'static, Result<StreamItem, ProviderError>>,
) -> BoxStream<'static, Result<ChatChunk, ProviderError>> {
    items
        .filter_map(|item| {
            futures::future::ready(match item {
                Ok(StreamItem::Chunk(chunk)) => Some(Ok(chunk)),
                Ok(StreamItem::Done) => None,
                Err(e) => Some(Err(e)),
            })
        })
        .boxed()
}

/// Prepare an OpenAI-compatible request for streaming: set `stream = true` and,
/// unless the client already set `stream_options`, ask the upstream to include
/// a final usage chunk (the ADR 003 token-accounting hook). Never overrides a
/// client-provided `stream_options`.
pub fn enable_stream_usage(req: &mut ChatRequest) {
    req.stream = true;
    req.extra
        .entry("stream_options".to_owned())
        .or_insert_with(|| serde_json::json!({ "include_usage": true }));
}

/// Turn a full [`ChatResponse`] into a single-frame chunk stream.
///
/// Each choice's complete message becomes one delta carrying the whole content
/// (and any `extra` fields such as `tool_calls`), preserving `finish_reason`
/// and `usage`. Valid OpenAI streaming shape, just not incremental.
#[must_use]
pub fn single_shot_stream(
    resp: ChatResponse,
) -> BoxStream<'static, Result<ChatChunk, ProviderError>> {
    let chunk = ChatChunk {
        id: resp.id,
        object: "chat.completion.chunk".to_owned(),
        created: resp.created,
        model: resp.model,
        choices: resp
            .choices
            .into_iter()
            .map(|c| ChatChunkChoice {
                index: c.index,
                delta: ChatDelta {
                    role: Some(c.message.role),
                    // `ChatDelta.content` stays a plain string (OpenAI streaming
                    // deltas are never multipart); collapse any image parts to
                    // their text, matching the non-streaming `.text()` idiom.
                    content: c.message.content.map(|c| c.text().into_owned()),
                    extra: c.message.extra,
                },
                finish_reason: c.finish_reason,
            })
            .collect(),
        usage: resp.usage,
    };
    stream::once(async move { Ok(chunk) }).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{ChatChoice, ChatMessage, MessageContent, Usage};

    fn response() -> ChatResponse {
        ChatResponse {
            id: "cmpl-1".to_owned(),
            object: "chat.completion".to_owned(),
            created: 0,
            model: "m".to_owned(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_owned(),
                    content: Some(MessageContent::Text("hello".to_owned())),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".to_owned()),
            }],
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
                estimated: None,
            }),
            extra: serde_json::Map::new(),
        }
    }

    fn empty_request() -> ChatRequest {
        ChatRequest {
            model: "m".to_owned(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn enable_stream_usage_sets_stream_and_default_usage_option() {
        let mut req = empty_request();
        enable_stream_usage(&mut req);
        assert!(req.stream);
        assert_eq!(req.extra["stream_options"]["include_usage"], true);
    }

    #[test]
    fn enable_stream_usage_never_overrides_client_stream_options() {
        let mut req = empty_request();
        req.extra.insert(
            "stream_options".to_owned(),
            serde_json::json!({ "include_usage": false }),
        );
        enable_stream_usage(&mut req);
        // The client's explicit choice is preserved.
        assert_eq!(req.extra["stream_options"]["include_usage"], false);
    }

    #[tokio::test]
    async fn emits_one_chunk_carrying_content_finish_and_usage() {
        let chunks: Vec<_> = single_shot_stream(response()).collect().await;
        assert_eq!(chunks.len(), 1);
        let chunk = chunks.into_iter().next().unwrap().unwrap();
        assert_eq!(chunk.object, "chat.completion.chunk");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(chunk.usage.unwrap().total_tokens, 4);
    }
}
