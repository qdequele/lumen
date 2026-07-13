//! Anthropic streaming-event translation.
//!
//! Anthropic streams typed SSE events (`message_start`, `content_block_start`,
//! `content_block_delta`, `message_delta`, `message_stop`, ...) rather than
//! OpenAI chunks; this module translates them event by event. Translation
//! state is bounded — ids, usage counters and a content-block→tool-call index
//! map — never the accumulated text (the M4 spec's hard rule).
//!
//! Mapping:
//!
//! * `message_start` → the initial chunk (`delta.role = "assistant"`), and
//!   captures the message id, model and `input_tokens`;
//! * `content_block_start` (`tool_use`) → a `tool_calls` delta with the id,
//!   name and empty arguments (OpenAI tool-call indices are allocated in
//!   order of appearance, independent of Anthropic block indices);
//! * `content_block_delta` — `text_delta` → a content delta,
//!   `input_json_delta` → a `tool_calls` arguments delta;
//! * `message_delta` → the final chunk with the mapped `finish_reason` and
//!   full usage (`input_tokens` from `message_start` + `output_tokens` here);
//! * `message_stop` → the terminal marker (`data: [DONE]` on the byte path);
//! * `ping` and unknown events are ignored; `error` events surface as a
//!   [`ProviderError`] (only the upstream error *type* is propagated — never
//!   message bodies, which could echo prompt content).

use std::collections::HashMap;

use ferrogate_core::{ChatChunk, ChatChunkChoice, ChatDelta, ProviderError, Usage};
use serde::Deserialize;
use serde_json::json;

use super::map_finish_reason;
use crate::chat::{SseTranslator, StreamItem};
use crate::sse::SseEvent;

/// Translator state for one Anthropic stream. Bounded by construction.
pub(super) struct AnthropicTranslator {
    /// Message id from `message_start` (empty until then).
    id: String,
    /// Model reported by `message_start`, falling back to the requested one.
    model: String,
    /// `input_tokens` captured at `message_start`, reported in the final usage.
    input_tokens: u32,
    /// Anthropic content-block index → OpenAI tool-call index.
    tool_indices: HashMap<u64, u64>,
    /// Next OpenAI tool-call index to allocate.
    next_tool_index: u64,
    /// Set once `message_stop` was seen; later events are ignored.
    finished: bool,
}

impl AnthropicTranslator {
    pub(super) fn new(requested_model: &str) -> Self {
        Self {
            id: String::new(),
            model: requested_model.to_owned(),
            input_tokens: 0,
            tool_indices: HashMap::new(),
            next_tool_index: 0,
            finished: false,
        }
    }

    /// Build a chunk from the captured id/model plus the given parts.
    fn chunk(
        &self,
        delta: ChatDelta,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: 0, // Anthropic does not stream a creation timestamp.
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
}

impl SseTranslator for AnthropicTranslator {
    fn translate(&mut self, event: &SseEvent) -> Result<Vec<StreamItem>, ProviderError> {
        if self.finished || event.data.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: AnthropicEvent = serde_json::from_str(&event.data)
            .map_err(|e| ProviderError::Translation(format!("anthropic stream event: {e}")))?;

        match parsed {
            AnthropicEvent::MessageStart { message } => {
                self.id = message.id;
                if !message.model.is_empty() {
                    self.model = message.model;
                }
                self.input_tokens = message.usage.input_tokens;
                Ok(vec![StreamItem::Chunk(self.chunk(
                    ChatDelta {
                        role: Some("assistant".to_owned()),
                        content: Some(String::new()),
                        extra: serde_json::Map::new(),
                    },
                    None,
                    None,
                ))])
            }
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                ContentBlock::ToolUse { id, name } => {
                    let tool_index = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.tool_indices.insert(index, tool_index);
                    let delta = Self::tool_calls_delta(json!({
                        "index": tool_index,
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" },
                    }));
                    Ok(vec![StreamItem::Chunk(self.chunk(delta, None, None))])
                }
                // Text blocks open silently; their text arrives as deltas.
                ContentBlock::Other => Ok(Vec::new()),
            },
            AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                BlockDelta::TextDelta { text } => Ok(vec![StreamItem::Chunk(self.chunk(
                    ChatDelta {
                        role: None,
                        content: Some(text),
                        extra: serde_json::Map::new(),
                    },
                    None,
                    None,
                ))]),
                BlockDelta::InputJsonDelta { partial_json } => {
                    let Some(&tool_index) = self.tool_indices.get(&index) else {
                        // A JSON delta for a block we never saw open: broken stream.
                        return Err(ProviderError::Translation(
                            "anthropic stream: input_json_delta for unknown block".to_owned(),
                        ));
                    };
                    let delta = Self::tool_calls_delta(json!({
                        "index": tool_index,
                        "function": { "arguments": partial_json },
                    }));
                    Ok(vec![StreamItem::Chunk(self.chunk(delta, None, None))])
                }
                // thinking/signature deltas etc. have no OpenAI equivalent.
                BlockDelta::Other => Ok(Vec::new()),
            },
            AnthropicEvent::MessageDelta { delta, usage } => {
                let completion = usage.output_tokens;
                let usage = Usage {
                    prompt_tokens: self.input_tokens,
                    completion_tokens: completion,
                    total_tokens: self.input_tokens.saturating_add(completion),
                    estimated: None,
                };
                Ok(vec![StreamItem::Chunk(self.chunk(
                    ChatDelta::default(),
                    map_finish_reason(delta.stop_reason.as_deref()),
                    Some(usage),
                ))])
            }
            AnthropicEvent::MessageStop => {
                self.finished = true;
                Ok(vec![StreamItem::Done])
            }
            AnthropicEvent::Error { error } => Err(ProviderError::Translation(format!(
                "anthropic stream error event: {}",
                error.error_type
            ))),
            AnthropicEvent::Ignored => Ok(Vec::new()),
        }
    }
}

// ---- Wire types (streaming events) ----------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicEvent {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: u64,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u64,
        delta: BlockDelta,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: DeltaUsage,
    },
    MessageStop,
    Error {
        error: ErrorBody,
    },
    /// `ping`, `content_block_stop`, and any future event type.
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct MessageStart {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: StartUsage,
}

#[derive(Default, Deserialize)]
struct StartUsage {
    #[serde(default)]
    input_tokens: u32,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Default, Deserialize)]
struct MessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct DeltaUsage {
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(rename = "type", default)]
    error_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // `json!` literals read better passed by value in test fixtures.
    #[allow(clippy::needless_pass_by_value)]
    fn event(name: &str, data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: Some(name.to_owned()),
            data: data.to_string(),
        }
    }

    fn feed(translator: &mut AnthropicTranslator, events: &[SseEvent]) -> Vec<StreamItem> {
        events
            .iter()
            .flat_map(|e| translator.translate(e).expect("translates"))
            .collect()
    }

    /// The criterion-4 fixture: a text + tool_use stream translates to the
    /// expected OpenAI chunk sequence.
    // One long, explicit fixture beats several fragmented ones here.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn full_stream_with_tool_use_translates_to_openai_chunks() {
        let mut t = AnthropicTranslator::new("claude-req");
        let items = feed(
            &mut t,
            &[
                event(
                    "message_start",
                    serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg_abc", "model": "claude-3-5-sonnet",
                            "usage": { "input_tokens": 25 }
                        }
                    }),
                ),
                event(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start", "index": 0,
                        "content_block": { "type": "text", "text": "" }
                    }),
                ),
                event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta", "index": 0,
                        "delta": { "type": "text_delta", "text": "Let me check." }
                    }),
                ),
                event(
                    "content_block_stop",
                    serde_json::json!({ "type": "content_block_stop", "index": 0 }),
                ),
                event(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start", "index": 1,
                        "content_block": {
                            "type": "tool_use", "id": "toolu_1", "name": "get_weather"
                        }
                    }),
                ),
                event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta", "index": 1,
                        "delta": { "type": "input_json_delta", "partial_json": "{\"city\":" }
                    }),
                ),
                event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta", "index": 1,
                        "delta": { "type": "input_json_delta", "partial_json": "\"Paris\"}" }
                    }),
                ),
                event(
                    "message_delta",
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "tool_use" },
                        "usage": { "output_tokens": 17 }
                    }),
                ),
                event(
                    "message_stop",
                    serde_json::json!({ "type": "message_stop" }),
                ),
            ],
        );

        // role chunk, text delta, tool open, 2 arg deltas, finish, Done.
        assert_eq!(items.len(), 7);

        let StreamItem::Chunk(role) = &items[0] else {
            panic!("expected chunk")
        };
        assert_eq!(role.id, "msg_abc");
        assert_eq!(role.model, "claude-3-5-sonnet");
        assert_eq!(role.object, "chat.completion.chunk");
        assert_eq!(role.choices[0].delta.role.as_deref(), Some("assistant"));

        let StreamItem::Chunk(text) = &items[1] else {
            panic!("expected chunk")
        };
        assert_eq!(
            text.choices[0].delta.content.as_deref(),
            Some("Let me check.")
        );

        let StreamItem::Chunk(tool_open) = &items[2] else {
            panic!("expected chunk")
        };
        let calls = &tool_open.choices[0].delta.extra["tool_calls"];
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "");

        let StreamItem::Chunk(arg1) = &items[3] else {
            panic!("expected chunk")
        };
        let calls = &arg1.choices[0].delta.extra["tool_calls"];
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["function"]["arguments"], "{\"city\":");

        let StreamItem::Chunk(finish) = &items[5] else {
            panic!("expected chunk")
        };
        assert_eq!(
            finish.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        let usage = finish.usage.expect("usage on final chunk");
        assert_eq!(usage.prompt_tokens, 25);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.total_tokens, 42);

        assert_eq!(items[6], StreamItem::Done);
    }

    #[test]
    fn tool_call_indices_are_allocated_in_order_of_appearance() {
        // Anthropic block indices 3 and 7 → OpenAI tool indices 0 and 1.
        let mut t = AnthropicTranslator::new("m");
        let items = feed(
            &mut t,
            &[
                event(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start", "index": 3,
                        "content_block": { "type": "tool_use", "id": "a", "name": "f" }
                    }),
                ),
                event(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start", "index": 7,
                        "content_block": { "type": "tool_use", "id": "b", "name": "g" }
                    }),
                ),
                event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta", "index": 7,
                        "delta": { "type": "input_json_delta", "partial_json": "{}" }
                    }),
                ),
            ],
        );
        let StreamItem::Chunk(second_args) = &items[2] else {
            panic!("expected chunk")
        };
        assert_eq!(
            second_args.choices[0].delta.extra["tool_calls"][0]["index"],
            1
        );
    }

    #[test]
    fn ping_and_unknown_events_are_ignored() {
        let mut t = AnthropicTranslator::new("m");
        let items = feed(
            &mut t,
            &[
                event("ping", serde_json::json!({ "type": "ping" })),
                event(
                    "some_future_event",
                    serde_json::json!({ "type": "some_future_event", "x": 1 }),
                ),
            ],
        );
        assert!(items.is_empty());
    }

    #[test]
    fn error_event_surfaces_type_but_never_the_message() {
        let mut t = AnthropicTranslator::new("m");
        let err = t
            .translate(&event(
                "error",
                serde_json::json!({
                    "type": "error",
                    "error": { "type": "overloaded_error", "message": "SECRET DETAIL" }
                }),
            ))
            .expect_err("error event fails the stream");
        let text = err.to_string();
        assert!(text.contains("overloaded_error"));
        assert!(!text.contains("SECRET DETAIL"));
    }

    #[test]
    fn malformed_event_json_is_a_translation_error() {
        let mut t = AnthropicTranslator::new("m");
        let err = t
            .translate(&SseEvent {
                event: Some("message_start".to_owned()),
                data: "{not json".to_owned(),
            })
            .expect_err("malformed JSON fails");
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    #[test]
    fn events_after_message_stop_are_ignored() {
        let mut t = AnthropicTranslator::new("m");
        let _ = t
            .translate(&event(
                "message_stop",
                serde_json::json!({ "type": "message_stop" }),
            ))
            .expect("stop ok");
        let items = t
            .translate(&event(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "late" }
                }),
            ))
            .expect("ignored");
        assert!(items.is_empty());
    }
}
