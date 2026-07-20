//! Bedrock Converse-stream event translation.
//!
//! `converse-stream` emits AWS event-stream frames (decoded by
//! [`super::eventstream`]); each frame's `:event-type` names a Converse event
//! whose JSON payload this module maps to an OpenAI [`ChatChunk`]. Translation
//! state is bounded (ids, a tool-block index map, captured input tokens) - the
//! response text is never accumulated.
//!
//! Event mapping:
//!
//! * `messageStart` (`{"role":"assistant"}`) -> the initial role chunk;
//! * `contentBlockStart` with a `toolUse` start -> a `tool_calls` delta opening
//!   the call (id, name, empty arguments);
//! * `contentBlockDelta` -> a content delta (`delta.text`) or a `tool_calls`
//!   arguments delta (`delta.toolUse.input`);
//! * `messageStop` (`{"stopReason":...}`) -> the final chunk with the mapped
//!   `finish_reason`;
//! * `metadata` (`{"usage":{...}}`) -> a usage chunk (ADR 003) followed by the
//!   terminal marker (`data: [DONE]` on the byte path);
//! * `exception` frames surface as a [`ProviderError`] carrying only the
//!   exception TYPE, never the upstream message (which could echo prompt text).

use std::collections::HashMap;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lumen_core::{ChatChunk, ChatChunkChoice, ChatDelta, ProviderError, Usage};
use serde::Deserialize;
use serde_json::json;

use super::eventstream::{EventMessage, EventStreamDecoder};
use super::map_finish_reason;
use crate::chat::StreamItem;

/// Translator state for one Converse stream. Bounded by construction.
pub(super) struct BedrockStreamTranslator {
    /// Synthesized OpenAI response id (Converse streams carry none).
    id: String,
    /// Requested model id, echoed on every chunk.
    model: String,
    /// Creation timestamp echoed on every chunk.
    created: u64,
    /// Converse `contentBlockIndex` -> OpenAI tool-call index.
    tool_indices: HashMap<u64, u64>,
    /// Next OpenAI tool-call index to allocate.
    next_tool_index: u64,
    /// Set once a terminal marker was emitted; later events are ignored.
    finished: bool,
}

impl BedrockStreamTranslator {
    pub(super) fn new(id: String, model: &str, created: u64) -> Self {
        Self {
            id,
            model: model.to_owned(),
            created,
            tool_indices: HashMap::new(),
            next_tool_index: 0,
            finished: false,
        }
    }

    /// Build a chunk from the captured id/model/created plus the given parts.
    fn chunk(
        &self,
        delta: ChatDelta,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage,
        }
    }

    /// A delta carrying only a `tool_calls` array.
    fn tool_calls_delta(entry: serde_json::Value) -> ChatDelta {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tool_calls".to_owned(),
            serde_json::Value::Array(vec![entry]),
        );
        ChatDelta {
            role: None,
            content: None,
            extra,
        }
    }

    /// Translate one decoded event-stream message into zero or more items.
    // One event-type dispatch; splitting it would only scatter the mapping.
    #[allow(clippy::too_many_lines)]
    pub(super) fn translate(
        &mut self,
        message: &EventMessage,
    ) -> Result<Vec<StreamItem>, ProviderError> {
        if self.finished {
            return Ok(Vec::new());
        }

        // Exception frames are surfaced by TYPE only - never the payload body,
        // which may echo prompt content. The stream is latched finished so any
        // trailing frames after the exception are ignored.
        if message.message_type() == Some("exception") {
            self.finished = true;
            let kind = message.exception_type().unwrap_or("unknown");
            return Err(ProviderError::Translation(format!(
                "bedrock stream exception: {kind}"
            )));
        }

        let Some(event_type) = message.event_type() else {
            return Ok(Vec::new());
        };
        // Empty payloads (e.g. contentBlockStop) parse as an empty object.
        let payload = if message.payload.is_empty() {
            b"{}".as_slice()
        } else {
            &message.payload
        };

        match event_type {
            "messageStart" => Ok(vec![StreamItem::Chunk(self.chunk(
                ChatDelta {
                    role: Some("assistant".to_owned()),
                    content: Some(String::new()),
                    extra: serde_json::Map::new(),
                },
                None,
                None,
            ))]),
            "contentBlockStart" => {
                let ev: ContentBlockStart = parse(payload)?;
                match ev.start.and_then(|s| s.tool_use) {
                    Some(tool) => {
                        let tool_index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.tool_indices.insert(ev.content_block_index, tool_index);
                        let delta = Self::tool_calls_delta(json!({
                            "index": tool_index,
                            "id": tool.tool_use_id,
                            "type": "function",
                            "function": { "name": tool.name, "arguments": "" },
                        }));
                        Ok(vec![StreamItem::Chunk(self.chunk(delta, None, None))])
                    }
                    // A text block opening carries no delta; text arrives later.
                    None => Ok(Vec::new()),
                }
            }
            "contentBlockDelta" => {
                let ev: ContentBlockDelta = parse(payload)?;
                let Some(delta) = ev.delta else {
                    return Ok(Vec::new());
                };
                if let Some(text) = delta.text {
                    Ok(vec![StreamItem::Chunk(self.chunk(
                        ChatDelta {
                            role: None,
                            content: Some(text),
                            extra: serde_json::Map::new(),
                        },
                        None,
                        None,
                    ))])
                } else if let Some(tool) = delta.tool_use {
                    let Some(&tool_index) = self.tool_indices.get(&ev.content_block_index) else {
                        return Err(ProviderError::Translation(
                            "bedrock stream: toolUse delta for unknown block".to_owned(),
                        ));
                    };
                    let delta = Self::tool_calls_delta(json!({
                        "index": tool_index,
                        "function": { "arguments": tool.input },
                    }));
                    Ok(vec![StreamItem::Chunk(self.chunk(delta, None, None))])
                } else {
                    Ok(Vec::new())
                }
            }
            "messageStop" => {
                let ev: MessageStop = parse(payload)?;
                Ok(vec![StreamItem::Chunk(self.chunk(
                    ChatDelta::default(),
                    map_finish_reason(ev.stop_reason.as_deref()),
                    None,
                ))])
            }
            "metadata" => {
                let ev: Metadata = parse(payload)?;
                let usage = ev.usage.map(|u| Usage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u
                        .total_tokens
                        .unwrap_or_else(|| u.input_tokens.saturating_add(u.output_tokens)),
                    estimated: None,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                });
                self.finished = true;
                Ok(vec![
                    StreamItem::Chunk(self.chunk(ChatDelta::default(), None, usage)),
                    StreamItem::Done,
                ])
            }
            // contentBlockStop and any future event have no OpenAI equivalent.
            _ => Ok(Vec::new()),
        }
    }
}

/// Parse a Converse event payload, mapping failures to a translation error
/// that never embeds the raw bytes.
fn parse<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(payload)
        .map_err(|e| ProviderError::Translation(format!("bedrock stream event: {e}")))
}

/// Pipe an upstream byte stream through the event-stream decoder and a
/// translator, yielding translated [`StreamItem`]s. Mirrors the SSE pipeline in
/// [`crate::chat::translate_sse_stream`] but for AWS binary framing.
pub(super) fn translate_eventstream(
    bytes: BoxStream<'static, Result<Bytes, ProviderError>>,
    translator: BedrockStreamTranslator,
) -> BoxStream<'static, Result<StreamItem, ProviderError>> {
    bytes
        .scan(
            (EventStreamDecoder::new(), translator),
            |(decoder, translator), item| {
                let out: Vec<Result<StreamItem, ProviderError>> = match item {
                    Ok(chunk) => match decoder.push(&chunk) {
                        Ok(messages) => messages
                            .iter()
                            .flat_map(|m| match translator.translate(m) {
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

// ---- Wire types (streaming event payloads) --------------------------------

#[derive(Deserialize)]
struct ContentBlockStart {
    #[serde(default)]
    start: Option<BlockStart>,
    #[serde(rename = "contentBlockIndex", default)]
    content_block_index: u64,
}

#[derive(Deserialize)]
struct BlockStart {
    #[serde(rename = "toolUse", default)]
    tool_use: Option<ToolUseStart>,
}

#[derive(Deserialize)]
struct ToolUseStart {
    #[serde(rename = "toolUseId", default)]
    tool_use_id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct ContentBlockDelta {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(rename = "contentBlockIndex", default)]
    content_block_index: u64,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "toolUse", default)]
    tool_use: Option<ToolUseDelta>,
}

#[derive(Deserialize)]
struct ToolUseDelta {
    #[serde(default)]
    input: String,
}

#[derive(Deserialize)]
struct MessageStop {
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default)]
    usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
// Wire type mirroring the Converse `usage` object; the shared `tokens` suffix
// is the API's own naming, not an internal smell.
#[allow(clippy::struct_field_names)]
struct StreamUsage {
    #[serde(rename = "inputTokens", default)]
    input_tokens: u32,
    #[serde(rename = "outputTokens", default)]
    output_tokens: u32,
    #[serde(rename = "totalTokens", default)]
    total_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::super::eventstream::test_support::event_frame;
    use super::*;

    fn translator() -> BedrockStreamTranslator {
        BedrockStreamTranslator::new("chatcmpl-x".to_owned(), "anthropic.claude", 42)
    }

    fn decode_one(bytes: &[u8]) -> EventMessage {
        let mut d = EventStreamDecoder::new();
        d.push(bytes).expect("decodes").into_iter().next().unwrap()
    }

    fn feed(t: &mut BedrockStreamTranslator, frames: &[Vec<u8>]) -> Vec<StreamItem> {
        frames
            .iter()
            .flat_map(|f| t.translate(&decode_one(f)).expect("translates"))
            .collect()
    }

    #[test]
    fn full_text_stream_translates_to_openai_chunks_with_usage() {
        let mut t = translator();
        let items = feed(
            &mut t,
            &[
                event_frame("messageStart", r#"{"role":"assistant"}"#),
                event_frame(
                    "contentBlockDelta",
                    r#"{"delta":{"text":"Hello"},"contentBlockIndex":0}"#,
                ),
                event_frame(
                    "contentBlockDelta",
                    r#"{"delta":{"text":" world"},"contentBlockIndex":0}"#,
                ),
                event_frame("contentBlockStop", r#"{"contentBlockIndex":0}"#),
                event_frame("messageStop", r#"{"stopReason":"end_turn"}"#),
                event_frame(
                    "metadata",
                    r#"{"usage":{"inputTokens":9,"outputTokens":3,"totalTokens":12}}"#,
                ),
            ],
        );

        // role, "Hello", " world", finish, usage, Done.
        assert_eq!(items.len(), 6);
        let StreamItem::Chunk(role) = &items[0] else {
            panic!("expected chunk")
        };
        assert_eq!(role.id, "chatcmpl-x");
        assert_eq!(role.model, "anthropic.claude");
        assert_eq!(role.created, 42);
        assert_eq!(role.choices[0].delta.role.as_deref(), Some("assistant"));

        let StreamItem::Chunk(hello) = &items[1] else {
            panic!("expected chunk")
        };
        assert_eq!(hello.choices[0].delta.content.as_deref(), Some("Hello"));

        let StreamItem::Chunk(finish) = &items[3] else {
            panic!("expected chunk")
        };
        assert_eq!(finish.choices[0].finish_reason.as_deref(), Some("stop"));

        let StreamItem::Chunk(usage_chunk) = &items[4] else {
            panic!("expected chunk")
        };
        let usage = usage_chunk.usage.expect("usage on metadata chunk");
        assert_eq!(usage.prompt_tokens, 9);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.total_tokens, 12);

        assert_eq!(items[5], StreamItem::Done);
    }

    #[test]
    fn tool_use_stream_maps_to_tool_call_deltas() {
        let mut t = translator();
        let items = feed(
            &mut t,
            &[
                event_frame(
                    "contentBlockStart",
                    r#"{"start":{"toolUse":{"toolUseId":"tu_1","name":"get_weather"}},"contentBlockIndex":1}"#,
                ),
                event_frame(
                    "contentBlockDelta",
                    r#"{"delta":{"toolUse":{"input":"{\"city\":"}},"contentBlockIndex":1}"#,
                ),
                event_frame(
                    "contentBlockDelta",
                    r#"{"delta":{"toolUse":{"input":"\"Paris\"}"}},"contentBlockIndex":1}"#,
                ),
            ],
        );
        assert_eq!(items.len(), 3);
        let StreamItem::Chunk(open) = &items[0] else {
            panic!("expected chunk")
        };
        let call = &open.choices[0].delta.extra["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "tu_1");
        assert_eq!(call["function"]["name"], "get_weather");
        assert_eq!(call["function"]["arguments"], "");

        let StreamItem::Chunk(arg1) = &items[1] else {
            panic!("expected chunk")
        };
        assert_eq!(
            arg1.choices[0].delta.extra["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );
    }

    #[test]
    fn exception_frame_surfaces_type_but_not_body() {
        let mut t = translator();
        let frame = super::super::eventstream::test_support::frame(
            &[
                (":exception-type", "modelStreamErrorException"),
                (":message-type", "exception"),
            ],
            br#"{"message":"SECRET UPSTREAM DETAIL"}"#,
        );
        let err = t
            .translate(&decode_one(&frame))
            .expect_err("exception fails");
        let text = err.to_string();
        assert!(text.contains("modelStreamErrorException"));
        assert!(!text.contains("SECRET UPSTREAM DETAIL"));
    }

    #[test]
    fn events_after_an_exception_are_ignored() {
        let mut t = translator();
        let exception = super::super::eventstream::test_support::frame(
            &[
                (":exception-type", "throttlingException"),
                (":message-type", "exception"),
            ],
            br#"{"message":"slow down"}"#,
        );
        let _ = t
            .translate(&decode_one(&exception))
            .expect_err("exception fails the stream");
        // The stream is latched: trailing frames produce nothing.
        let late = t
            .translate(&decode_one(&event_frame(
                "contentBlockDelta",
                r#"{"delta":{"text":"late"},"contentBlockIndex":0}"#,
            )))
            .expect("ignored after exception");
        assert!(late.is_empty());
    }

    #[test]
    fn events_after_metadata_are_ignored() {
        let mut t = translator();
        let _ = feed(
            &mut t,
            &[event_frame(
                "metadata",
                r#"{"usage":{"inputTokens":1,"outputTokens":1}}"#,
            )],
        );
        let late = t
            .translate(&decode_one(&event_frame(
                "contentBlockDelta",
                r#"{"delta":{"text":"late"},"contentBlockIndex":0}"#,
            )))
            .expect("ignored");
        assert!(late.is_empty());
    }
}
