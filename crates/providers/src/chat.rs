//! Shared chat helpers.
//!
//! [`single_shot_stream`] adapts a complete [`ChatResponse`] into a one-frame
//! [`ChatChunk`] stream. It is the interim `chat_stream` implementation for the
//! non-streaming M4 slice: `stream=true` works (one chunk + `[DONE]`), but the
//! bytes are not incremental. The M4 streaming slice replaces per-provider
//! `chat_stream` with genuine incremental SSE (zero-copy passthrough where the
//! upstream already speaks OpenAI; chunk-by-chunk translation for Anthropic).

use ferrogate_core::{
    ChatChunk, ChatChunkChoice, ChatDelta, ChatRequest, ChatResponse, ProviderError,
};
use futures::stream::{self, BoxStream, StreamExt};

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
                    content: c.message.content,
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
    use ferrogate_core::{ChatChoice, ChatMessage, Usage};

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
                    content: Some("hello".to_owned()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".to_owned()),
            }],
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
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
