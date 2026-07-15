//! Cohere v2 streaming-event translation.
//!
//! Cohere's v2 chat stream is typed SSE, one event per line-delimited
//! `data:` frame with a `type` discriminator (`message-start`,
//! `content-delta`, `tool-call-start`, `tool-call-delta`, `message-end`, ...) -
//! structurally close to Anthropic's event stream, translated event by event
//! here. Translation state is bounded - the message id, model and a content-
//! block/tool-call index map - never the accumulated text (the M4 spec's hard
//! rule, followed here as it was for Anthropic).
//!
//! Mapping:
//!
//! * `message-start` -> the initial chunk (`delta.role = "assistant"`),
//!   capturing the message id;
//! * `content-delta` -> a content delta (Cohere's `content-start`/
//!   `content-end` bracket a text block but carry no data of their own and
//!   are ignored, like Anthropic's silent block-open);
//! * `tool-call-start` -> a `tool_calls` delta with the id, name and empty
//!   arguments (OpenAI tool-call indices are allocated in order of
//!   appearance, independent of Cohere's block indices);
//! * `tool-call-delta` -> a `tool_calls` arguments delta;
//! * `tool-plan-delta` (Cohere's pre-tool-call reasoning trace) has no OpenAI
//!   equivalent and is dropped, like Anthropic's `thinking` deltas;
//! * `message-end` carries BOTH the finish reason and the full usage in one
//!   event (unlike Anthropic's separate `message_delta` + `message_stop`) and
//!   is this stream's sole terminal event: it yields the final chunk followed
//!   immediately by [`StreamItem::Done`];
//! * `citation-start`/`citation-end` and any future event type are ignored;
//!   an `error` event surfaces as a [`ProviderError`] (only the fact of an
//!   error propagates - never any upstream-provided message text, which could
//!   echo prompt content).

use std::collections::HashMap;

use lumen_core::{ChatChunk, ChatChunkChoice, ChatDelta, ProviderError};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::chat::{map_finish_reason, usage_from_cohere, CohereUsage};
use crate::chat::{SseTranslator, StreamItem};
use crate::sse::SseEvent;

/// Translator state for one Cohere v2 stream. Bounded by construction.
pub(super) struct CohereTranslator {
    /// Message id from `message-start` (empty until then).
    id: String,
    /// The requested model (Cohere does not echo a model id in stream events).
    model: String,
    /// Cohere tool-call index -> OpenAI tool-call index.
    tool_indices: HashMap<u64, u64>,
    /// Next OpenAI tool-call index to allocate.
    next_tool_index: u64,
    /// Set once `message-end` was seen; later events are ignored.
    finished: bool,
}

impl CohereTranslator {
    pub(super) fn new(requested_model: &str) -> Self {
        Self {
            id: String::new(),
            model: requested_model.to_owned(),
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
        usage: Option<lumen_core::Usage>,
    ) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: 0, // Cohere does not stream a creation timestamp.
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
    fn tool_calls_delta(entry: Value) -> ChatDelta {
        let mut extra = Map::new();
        extra.insert("tool_calls".to_owned(), Value::Array(vec![entry]));
        ChatDelta {
            role: None,
            content: None,
            extra,
        }
    }
}

impl SseTranslator for CohereTranslator {
    fn translate(&mut self, event: &SseEvent) -> Result<Vec<StreamItem>, ProviderError> {
        if self.finished || event.data.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: CohereEvent = serde_json::from_str(&event.data)
            .map_err(|e| ProviderError::Translation(format!("cohere stream event: {e}")))?;

        match parsed {
            CohereEvent::MessageStart { id } => {
                self.id = id;
                Ok(vec![StreamItem::Chunk(self.chunk(
                    ChatDelta {
                        role: Some("assistant".to_owned()),
                        content: Some(String::new()),
                        extra: Map::new(),
                    },
                    None,
                    None,
                ))])
            }
            CohereEvent::ContentDelta { delta, .. } => Ok(vec![StreamItem::Chunk(self.chunk(
                ChatDelta {
                    role: None,
                    content: Some(delta.message.content.text),
                    extra: Map::new(),
                },
                None,
                None,
            ))]),
            CohereEvent::ToolCallStart { index, delta } => {
                let tool_index = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_indices.insert(index, tool_index);
                let call = delta.message.tool_calls;
                let entry = json!({
                    "index": tool_index,
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.function.name, "arguments": call.function.arguments },
                });
                Ok(vec![StreamItem::Chunk(self.chunk(
                    Self::tool_calls_delta(entry),
                    None,
                    None,
                ))])
            }
            CohereEvent::ToolCallDelta { index, delta } => {
                let Some(&tool_index) = self.tool_indices.get(&index) else {
                    // An arguments delta for a block we never saw open: broken stream.
                    return Err(ProviderError::Translation(
                        "cohere stream: tool-call-delta for unknown index".to_owned(),
                    ));
                };
                let entry = json!({
                    "index": tool_index,
                    "function": { "arguments": delta.message.tool_calls.function.arguments },
                });
                Ok(vec![StreamItem::Chunk(self.chunk(
                    Self::tool_calls_delta(entry),
                    None,
                    None,
                ))])
            }
            CohereEvent::MessageEnd { delta } => {
                self.finished = true;
                let usage = delta.usage.map(usage_from_cohere);
                let finish_reason = map_finish_reason(delta.finish_reason.as_deref());
                Ok(vec![
                    StreamItem::Chunk(self.chunk(ChatDelta::default(), finish_reason, usage)),
                    StreamItem::Done,
                ])
            }
            CohereEvent::Error => Err(ProviderError::Translation(
                "cohere stream error event".to_owned(),
            )),
            CohereEvent::Ignored => Ok(Vec::new()),
        }
    }
}

// ---- Wire types (streaming events) ----------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum CohereEvent {
    MessageStart {
        #[serde(default)]
        id: String,
    },
    // `index` (which content block) is not needed: a Cohere text reply
    // streams on a single content index, so the delta text applies as-is.
    ContentDelta {
        delta: ContentDeltaWrapper,
    },
    ToolCallStart {
        index: u64,
        delta: ToolCallStartWrapper,
    },
    ToolCallDelta {
        index: u64,
        delta: ToolCallDeltaWrapper,
    },
    MessageEnd {
        #[serde(default)]
        delta: MessageEndDelta,
    },
    /// Cohere signals an in-stream failure; the message text is deliberately
    /// not captured (never surfaced - see the module docs).
    Error,
    /// `content-start`, `content-end`, `tool-plan-delta`, `tool-call-end`,
    /// `citation-start`, `citation-end`, and any future event type.
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct ContentDeltaWrapper {
    message: ContentDeltaMessage,
}

#[derive(Deserialize)]
struct ContentDeltaMessage {
    content: ContentDeltaContent,
}

#[derive(Default, Deserialize)]
struct ContentDeltaContent {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ToolCallStartWrapper {
    message: ToolCallStartMessage,
}

#[derive(Deserialize)]
struct ToolCallStartMessage {
    tool_calls: ToolCallStartCall,
}

#[derive(Deserialize)]
struct ToolCallStartCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: ToolCallFunction,
}

#[derive(Default, Deserialize)]
struct ToolCallFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct ToolCallDeltaWrapper {
    message: ToolCallDeltaMessage,
}

#[derive(Deserialize)]
struct ToolCallDeltaMessage {
    tool_calls: ToolCallDeltaCall,
}

#[derive(Deserialize)]
struct ToolCallDeltaCall {
    function: ToolCallDeltaFunction,
}

#[derive(Deserialize)]
struct ToolCallDeltaFunction {
    #[serde(default)]
    arguments: String,
}

#[derive(Default, Deserialize)]
struct MessageEndDelta {
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    usage: Option<CohereUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // `json!` literals read better passed by value in test fixtures.
    #[allow(clippy::needless_pass_by_value)]
    fn event(name: &str, data: Value) -> SseEvent {
        SseEvent {
            event: Some(name.to_owned()),
            data: data.to_string(),
        }
    }

    fn feed(translator: &mut CohereTranslator, events: &[SseEvent]) -> Vec<StreamItem> {
        events
            .iter()
            .flat_map(|e| translator.translate(e).expect("translates"))
            .collect()
    }

    /// A text + tool_use stream translates to the expected OpenAI chunk
    /// sequence, mirroring the Anthropic fixture test.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn full_stream_with_tool_call_translates_to_openai_chunks() {
        let mut t = CohereTranslator::new("command-r-plus");
        let items = feed(
            &mut t,
            &[
                event(
                    "message-start",
                    json!({ "type": "message-start", "id": "chat_abc" }),
                ),
                event(
                    "content-start",
                    json!({
                        "type": "content-start", "index": 0,
                        "delta": { "message": { "content": { "type": "text", "text": "" } } }
                    }),
                ),
                event(
                    "content-delta",
                    json!({
                        "type": "content-delta", "index": 0,
                        "delta": { "message": { "content": { "text": "Let me check." } } }
                    }),
                ),
                event("content-end", json!({ "type": "content-end", "index": 0 })),
                event(
                    "tool-call-start",
                    json!({
                        "type": "tool-call-start", "index": 1,
                        "delta": { "message": { "tool_calls": {
                            "id": "call_1", "type": "function",
                            "function": { "name": "get_weather", "arguments": "" }
                        } } }
                    }),
                ),
                event(
                    "tool-call-delta",
                    json!({
                        "type": "tool-call-delta", "index": 1,
                        "delta": { "message": { "tool_calls": {
                            "function": { "arguments": "{\"city\":" }
                        } } }
                    }),
                ),
                event(
                    "tool-call-delta",
                    json!({
                        "type": "tool-call-delta", "index": 1,
                        "delta": { "message": { "tool_calls": {
                            "function": { "arguments": "\"Paris\"}" }
                        } } }
                    }),
                ),
                event(
                    "message-end",
                    json!({
                        "type": "message-end",
                        "delta": {
                            "finish_reason": "TOOL_CALL",
                            "usage": {
                                "billed_units": { "input_tokens": 24, "output_tokens": 16 },
                                "tokens": { "input_tokens": 25, "output_tokens": 17 }
                            }
                        }
                    }),
                ),
            ],
        );

        // role chunk, text delta, tool open, 2 arg deltas, finish, Done.
        assert_eq!(items.len(), 7);

        let StreamItem::Chunk(role) = &items[0] else {
            panic!("expected chunk")
        };
        assert_eq!(role.id, "chat_abc");
        assert_eq!(role.model, "command-r-plus");
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
        assert_eq!(calls[0]["id"], "call_1");
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
        // `tokens` wins over `billed_units`.
        assert_eq!(usage.prompt_tokens, 25);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.total_tokens, 42);

        assert_eq!(items[6], StreamItem::Done);
    }

    #[test]
    fn tool_call_indices_are_allocated_in_order_of_appearance() {
        // Cohere block indices 3 and 7 -> OpenAI tool indices 0 and 1.
        let mut t = CohereTranslator::new("m");
        let items = feed(
            &mut t,
            &[
                event(
                    "tool-call-start",
                    json!({
                        "type": "tool-call-start", "index": 3,
                        "delta": { "message": { "tool_calls": {
                            "id": "a", "type": "function", "function": { "name": "f", "arguments": "" }
                        } } }
                    }),
                ),
                event(
                    "tool-call-start",
                    json!({
                        "type": "tool-call-start", "index": 7,
                        "delta": { "message": { "tool_calls": {
                            "id": "b", "type": "function", "function": { "name": "g", "arguments": "" }
                        } } }
                    }),
                ),
                event(
                    "tool-call-delta",
                    json!({
                        "type": "tool-call-delta", "index": 7,
                        "delta": { "message": { "tool_calls": { "function": { "arguments": "{}" } } } }
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
    fn tool_plan_and_citation_events_are_ignored() {
        let mut t = CohereTranslator::new("m");
        let items = feed(
            &mut t,
            &[
                event(
                    "tool-plan-delta",
                    json!({ "type": "tool-plan-delta", "delta": { "message": { "tool_plan": "thinking..." } } }),
                ),
                event(
                    "citation-start",
                    json!({ "type": "citation-start", "index": 0 }),
                ),
                event(
                    "citation-end",
                    json!({ "type": "citation-end", "index": 0 }),
                ),
                event(
                    "some_future_event",
                    json!({ "type": "some_future_event", "x": 1 }),
                ),
            ],
        );
        assert!(items.is_empty());
    }

    #[test]
    fn error_event_surfaces_but_never_a_message_body() {
        let mut t = CohereTranslator::new("m");
        let err = t
            .translate(&event(
                "error",
                json!({ "type": "error", "message": "SECRET DETAIL" }),
            ))
            .expect_err("error event fails the stream");
        let text = err.to_string();
        assert!(!text.contains("SECRET DETAIL"));
    }

    #[test]
    fn malformed_event_json_is_a_translation_error() {
        let mut t = CohereTranslator::new("m");
        let err = t
            .translate(&SseEvent {
                event: Some("message-start".to_owned()),
                data: "{not json".to_owned(),
            })
            .expect_err("malformed JSON fails");
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    #[test]
    fn tool_call_delta_for_unknown_index_is_a_translation_error() {
        let mut t = CohereTranslator::new("m");
        let err = t
            .translate(&event(
                "tool-call-delta",
                json!({
                    "type": "tool-call-delta", "index": 42,
                    "delta": { "message": { "tool_calls": { "function": { "arguments": "{}" } } } }
                }),
            ))
            .expect_err("unknown index fails");
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    #[test]
    fn events_after_message_end_are_ignored() {
        let mut t = CohereTranslator::new("m");
        let _ = t
            .translate(&event("message-end", json!({ "type": "message-end" })))
            .expect("message-end ok");
        let items = t
            .translate(&event(
                "content-delta",
                json!({
                    "type": "content-delta", "index": 0,
                    "delta": { "message": { "content": { "text": "late" } } }
                }),
            ))
            .expect("ignored");
        assert!(items.is_empty());
    }

    #[test]
    fn message_end_without_usage_or_finish_reason_still_terminates() {
        let mut t = CohereTranslator::new("m");
        let items = feed(
            &mut t,
            &[event("message-end", json!({ "type": "message-end" }))],
        );
        assert_eq!(items.len(), 2);
        let StreamItem::Chunk(finish) = &items[0] else {
            panic!("expected chunk")
        };
        assert_eq!(finish.choices[0].finish_reason, None);
        assert_eq!(finish.usage, None);
        assert_eq!(items[1], StreamItem::Done);
    }
}
