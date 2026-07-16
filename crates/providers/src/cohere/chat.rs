//! Cohere v2 chat request/response translation (Command R / R+, non-streaming
//! path).
//!
//! `POST /v2/chat` is closer to OpenAI's shape than Anthropic's Messages API:
//! `system`/`user`/`assistant`/`tool` roles live directly in `messages` (no
//! top-level `system` hoist), and an assistant's `tool_calls` already use the
//! OpenAI `{id, type: "function", function: {name, arguments}}` shape, so that
//! leg of translation is closer to identity. What differs:
//!
//! * request: `top_p` -> `p`, `max_tokens` is optional (Cohere does not
//!   require it, unlike Anthropic); `stop` (string or array) -> the array
//!   `stop_sequences`; `tool_choice` collapses to Cohere's `"REQUIRED"` /
//!   `"NONE"` strings (omitted for `"auto"`) - forcing one specific tool is
//!   not supported upstream and is dropped with a `debug` trace, falling back
//!   to `auto`;
//! * vision (issue #73, Command-A-Vision): a message with image parts becomes
//!   an array of v2 content blocks (`text` / `image_url`, OpenAI-shaped, see
//!   [`cohere_content`]); a text-only message keeps the plain-string fast
//!   path. Both remote URLs and `data:` URIs are accepted upstream, so the
//!   default `accepts_remote_image_url` (true) is correct for this kind;
//! * response: `message.content` is an array of typed blocks (only `type:
//!   "text"` is emitted for a pure-text reply) instead of a bare string;
//!   `finish_reason` is `COMPLETE`/`STOP_SEQUENCE`/`MAX_TOKENS`/`TOOL_CALL`/
//!   `ERROR`;
//! * usage: `usage.tokens` (the actual pre-billing token count, matching
//!   OpenAI's `prompt_tokens`/`completion_tokens` semantics) is preferred over
//!   `usage.billed_units` (what's charged, which can differ e.g. under
//!   caching discounts); a response reporting neither leaves `usage: None` so
//!   the gateway's local estimator fills in an honestly-flagged count
//!   (ADR 003).
//!
//! `n` (multiple completions) has no Cohere v2 equivalent and is dropped with
//! a `debug` trace rather than silently ignored.

use lumen_core::{ChatChoice, ChatMessage, ChatRequest, ChatResponse, MessageContent, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

// ---- Wire types (request) -------------------------------------------------

#[derive(Debug, Serialize)]
pub(super) struct CohereChatRequest {
    pub(super) model: String,
    pub(super) messages: Vec<CohereMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_choice: Option<Value>,
    /// Only serialized on the streaming path.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) stream: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct CohereMessage {
    pub(super) role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) tool_calls: Vec<Value>,
}

/// The text of a message's content (empty for a content-less, pure
/// `tool_calls` assistant message).
fn text_of(m: &ChatMessage) -> String {
    m.content
        .as_ref()
        .map(|c| c.text().into_owned())
        .unwrap_or_default()
}

/// Whether an assistant message carries OpenAI `tool_calls` in `extra`.
fn has_tool_calls(m: &ChatMessage) -> bool {
    m.extra.get("tool_calls").is_some_and(Value::is_array)
}

/// Build a Cohere v2 message `content`: a plain string when there are no
/// images (the fast path, and what text-only clients have always sent), else
/// an array of `text`/`image_url` blocks with part order preserved
/// (issue #73, Command-A-Vision).
///
/// Cohere v2's vision blocks are OpenAI-shaped -
/// `{"type":"image_url","image_url":{"url",...,"detail"?}}` - and the
/// upstream accepts both remote `http(s)` URLs (which Cohere fetches itself)
/// and inline `data:` URIs, so the [`lumen_core::ImageUrl`] serializes verbatim: no URL
/// rewriting, `detail` forwarded untouched. Provider-native references
/// (Anthropic `file_id`, Gemini `fileUri`) never reach this path: the
/// gateway's pre-flight rejects them for the cohere kind with `LM-2008`.
fn cohere_content(m: &ChatMessage) -> Value {
    match m.content.as_ref() {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    blocks.push(json!({ "type": "image_url", "image_url": img }));
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
            }
            Value::Array(blocks)
        }
        // No images (string, text-only parts, or none): a plain string.
        _ => Value::String(text_of(m)),
    }
}

/// Translate one OpenAI-shaped message to Cohere v2's shape.
fn translate_message(m: &ChatMessage) -> CohereMessage {
    match m.role.as_str() {
        "tool" => CohereMessage {
            role: "tool".to_owned(),
            content: Some(json!(text_of(m))),
            tool_call_id: m
                .extra
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_calls: Vec::new(),
        },
        "assistant" if has_tool_calls(m) => {
            let text = text_of(m);
            CohereMessage {
                role: "assistant".to_owned(),
                content: if text.is_empty() {
                    None
                } else {
                    Some(json!(text))
                },
                tool_call_id: None,
                tool_calls: m
                    .extra
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            }
        }
        role => CohereMessage {
            role: role.to_owned(),
            content: Some(cohere_content(m)),
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
    }
}

/// OpenAI `stop` is a string or array of strings; normalise to a list.
fn collect_stop_sequences(stop: &Value) -> Vec<String> {
    match stop {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// OpenAI `tools` (`{type: "function", function: {...}}`) pass through
/// unchanged - Cohere v2's function-tool shape is OpenAI-compatible. Non-
/// function entries are dropped (Cohere v2 only supports function tools).
fn translate_tools(req: &ChatRequest) -> Vec<Value> {
    let Some(tools) = req.extra.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
        .cloned()
        .collect()
}

/// OpenAI `tool_choice` -> Cohere v2 `tool_choice` (`"REQUIRED"` / `"NONE"`,
/// omitted for `auto`). Forcing one specific tool by name has no Cohere v2
/// equivalent and is dropped (falls back to the upstream default, `auto`).
fn translate_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(s) => match s.as_str() {
            "required" => Some(json!("REQUIRED")),
            "none" => Some(json!("NONE")),
            _ => None,
        },
        Value::Object(_) => {
            tracing::debug!(
                "cohere chat: forcing a specific tool via `tool_choice` is not supported by \
                 v2 chat; dropped (falls back to auto)"
            );
            None
        }
        _ => None,
    }
}

/// Build the Cohere v2 request body from an OpenAI-shaped [`ChatRequest`].
/// `stream` is set explicitly by the calling path, never taken from the
/// client's request (the gateway decides which upstream mode it needs).
pub(super) fn translate_request(req: &ChatRequest, stream: bool) -> CohereChatRequest {
    if req.n.is_some_and(|n| n > 1) {
        tracing::debug!(
            n = req.n,
            "cohere chat: `n` (multiple completions) has no v2 chat equivalent; only one \
             completion will be returned"
        );
    }
    CohereChatRequest {
        model: req.model.clone(),
        messages: req.messages.iter().map(translate_message).collect(),
        temperature: req.temperature,
        p: req.top_p,
        max_tokens: req.max_tokens,
        stop_sequences: req
            .stop
            .as_ref()
            .map(collect_stop_sequences)
            .unwrap_or_default(),
        tools: translate_tools(req),
        tool_choice: req.extra.get("tool_choice").and_then(translate_tool_choice),
        stream,
    }
}

// ---- Wire types (response) -------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct CohereChatResponse {
    #[serde(default)]
    pub(super) id: String,
    #[serde(default)]
    pub(super) finish_reason: Option<String>,
    #[serde(default)]
    pub(super) message: CohereResponseMessage,
    #[serde(default)]
    pub(super) usage: Option<CohereUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CohereResponseMessage {
    #[serde(default)]
    pub(super) content: Vec<CohereContentBlock>,
    #[serde(default)]
    pub(super) tool_calls: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CohereContentBlock {
    #[serde(rename = "type", default)]
    pub(super) block_type: String,
    #[serde(default)]
    pub(super) text: String,
}

/// Cohere's `usage` object: `tokens` (actual pre-billing counts, preferred)
/// and/or `billed_units` (what's charged; the fallback).
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(super) struct CohereUsage {
    #[serde(default)]
    pub(super) billed_units: Option<CohereUnitCounts>,
    #[serde(default)]
    pub(super) tokens: Option<CohereUnitCounts>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(super) struct CohereUnitCounts {
    #[serde(default)]
    pub(super) input_tokens: u32,
    #[serde(default)]
    pub(super) output_tokens: u32,
}

/// Translate a Cohere `usage` object to the internal [`Usage`] shape,
/// preferring `tokens` over `billed_units` (see module docs).
pub(super) fn usage_from_cohere(usage: CohereUsage) -> Usage {
    let counts = usage.tokens.or(usage.billed_units).unwrap_or_default();
    Usage {
        prompt_tokens: counts.input_tokens,
        completion_tokens: counts.output_tokens,
        total_tokens: counts.input_tokens.saturating_add(counts.output_tokens),
        estimated: None,
    }
}

/// Translate a Cohere `finish_reason` to an OpenAI `finish_reason`.
pub(super) fn map_finish_reason(finish_reason: Option<&str>) -> Option<String> {
    match finish_reason {
        Some("COMPLETE" | "STOP_SEQUENCE") => Some("stop".to_owned()),
        Some("MAX_TOKENS") => Some("length".to_owned()),
        Some("TOOL_CALL") => Some("tool_calls".to_owned()),
        Some(other) => Some(other.to_lowercase()),
        None => None,
    }
}

/// Build an OpenAI-shaped [`ChatResponse`] from a Cohere v2 response. Cohere
/// does not echo the model id in the response, so the requested one is used.
pub(super) fn translate_response(resp: CohereChatResponse, requested_model: &str) -> ChatResponse {
    let mut content = String::new();
    for block in &resp.message.content {
        if block.block_type == "text" || block.block_type.is_empty() {
            content.push_str(&block.text);
        }
    }

    let mut extra = Map::new();
    if !resp.message.tool_calls.is_empty() {
        extra.insert(
            "tool_calls".to_owned(),
            Value::Array(resp.message.tool_calls),
        );
    }
    // OpenAI uses `content: null` for a pure tool-call message.
    let content = if content.is_empty() && !extra.is_empty() {
        None
    } else {
        Some(MessageContent::Text(content))
    };

    ChatResponse {
        id: resp.id,
        object: "chat.completion".to_owned(),
        created: 0, // Cohere does not return a creation timestamp.
        model: requested_model.to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_owned(),
                content,
                name: None,
                extra,
            },
            finish_reason: map_finish_reason(resp.finish_reason.as_deref()),
        }],
        usage: resp.usage.map(usage_from_cohere),
        extra: Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::ContentPart;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: Some(MessageContent::Text(content.to_owned())),
            name: None,
            extra: Map::new(),
        }
    }

    fn base_request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "command-r-plus".to_owned(),
            messages,
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn request_maps_top_p_to_p_and_stop_to_stop_sequences() {
        let mut req = base_request(vec![msg("user", "hi")]);
        req.top_p = Some(0.8);
        req.max_tokens = Some(256);
        req.stop = Some(json!(["STOP", "END"]));
        let out = translate_request(&req, false);
        assert_eq!(out.p, Some(0.8));
        assert_eq!(out.max_tokens, Some(256));
        assert_eq!(
            out.stop_sequences,
            vec!["STOP".to_owned(), "END".to_owned()]
        );
        assert!(!out.stream);
    }

    #[test]
    fn system_role_stays_inline_unlike_anthropic() {
        let req = base_request(vec![msg("system", "be terse"), msg("user", "hi")]);
        let out = translate_request(&req, false);
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0].role, "system");
        assert_eq!(out.messages[0].content, Some(json!("be terse")));
    }

    #[test]
    fn tool_message_carries_tool_call_id_and_text_content() {
        let mut extra = Map::new();
        extra.insert("tool_call_id".to_owned(), json!("call_1"));
        let tool_msg = ChatMessage {
            role: "tool".to_owned(),
            content: Some(MessageContent::Text("18C, sunny".to_owned())),
            name: None,
            extra,
        };
        let req = base_request(vec![tool_msg]);
        let out = translate_request(&req, false);
        assert_eq!(out.messages[0].role, "tool");
        assert_eq!(out.messages[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(out.messages[0].content, Some(json!("18C, sunny")));
    }

    #[test]
    fn assistant_tool_calls_pass_through_openai_shaped() {
        let mut extra = Map::new();
        extra.insert(
            "tool_calls".to_owned(),
            json!([{
                "id": "call_1",
                "type": "function",
                "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
            }]),
        );
        let assistant_msg = ChatMessage {
            role: "assistant".to_owned(),
            content: None,
            name: None,
            extra,
        };
        let req = base_request(vec![assistant_msg]);
        let out = translate_request(&req, false);
        assert_eq!(out.messages[0].content, None);
        assert_eq!(out.messages[0].tool_calls.len(), 1);
        assert_eq!(out.messages[0].tool_calls[0]["id"], "call_1");
        assert_eq!(
            out.messages[0].tool_calls[0]["function"]["name"],
            "get_weather"
        );
    }

    #[test]
    fn tool_choice_variants_map_to_cohere_shapes() {
        assert_eq!(
            translate_tool_choice(&json!("required")),
            Some(json!("REQUIRED"))
        );
        assert_eq!(translate_tool_choice(&json!("none")), Some(json!("NONE")));
        // `auto` and a forced-tool object both fall back to the upstream default.
        assert_eq!(translate_tool_choice(&json!("auto")), None);
        assert_eq!(
            translate_tool_choice(&json!({ "type": "function", "function": { "name": "f" } })),
            None
        );
    }

    #[test]
    fn tools_pass_through_openai_function_shape_unchanged() {
        let mut extra = Map::new();
        extra.insert(
            "tools".to_owned(),
            json!([{
                "type": "function",
                "function": { "name": "get_weather", "parameters": { "type": "object" } }
            }]),
        );
        let mut req = base_request(vec![msg("user", "hi")]);
        req.extra = extra;
        let out = translate_request(&req, false);
        assert_eq!(out.tools.len(), 1);
        assert_eq!(out.tools[0]["function"]["name"], "get_weather");
    }

    /// Issue #73: a message carrying image parts becomes an array of Cohere
    /// v2 content blocks - text parts as `{"type":"text",...}`, image parts
    /// as `{"type":"image_url","image_url":{...}}` - order preserved, the
    /// URL form (`data:` URI or remote) and the `detail` hint untouched.
    #[test]
    fn image_parts_become_cohere_v2_content_blocks() {
        let parts_msg = ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("what is this?".to_owned()),
                    image_url: None,
                    extra: Map::new(),
                },
                ContentPart {
                    kind: "image_url".to_owned(),
                    text: None,
                    image_url: Some(lumen_core::ImageUrl {
                        url: "data:image/png;base64,AAAA".to_owned(),
                        detail: Some("low".to_owned()),
                    }),
                    extra: Map::new(),
                },
                ContentPart {
                    kind: "image_url".to_owned(),
                    text: None,
                    image_url: Some(lumen_core::ImageUrl {
                        url: "https://example.com/cat.png".to_owned(),
                        detail: None,
                    }),
                    extra: Map::new(),
                },
            ])),
            name: None,
            extra: Map::new(),
        };
        let req = base_request(vec![parts_msg]);
        let out = translate_request(&req, false);
        assert_eq!(
            out.messages[0].content,
            Some(json!([
                { "type": "text", "text": "what is this?" },
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,AAAA", "detail": "low" }
                },
                {
                    "type": "image_url",
                    "image_url": { "url": "https://example.com/cat.png" }
                },
            ]))
        );
    }

    #[test]
    fn multipart_content_is_flattened_to_its_text() {
        let parts_msg = ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![ContentPart {
                kind: "text".to_owned(),
                text: Some("hello".to_owned()),
                image_url: None,
                extra: Map::new(),
            }])),
            name: None,
            extra: Map::new(),
        };
        let req = base_request(vec![parts_msg]);
        let out = translate_request(&req, false);
        assert_eq!(out.messages[0].content, Some(json!("hello")));
    }

    #[test]
    fn response_concatenates_text_blocks_and_maps_finish_reason() {
        let resp = CohereChatResponse {
            id: "chat_1".to_owned(),
            finish_reason: Some("MAX_TOKENS".to_owned()),
            message: CohereResponseMessage {
                content: vec![
                    CohereContentBlock {
                        block_type: "text".to_owned(),
                        text: "Hello ".to_owned(),
                    },
                    CohereContentBlock {
                        block_type: "text".to_owned(),
                        text: "world".to_owned(),
                    },
                ],
                tool_calls: Vec::new(),
            },
            usage: Some(CohereUsage {
                billed_units: Some(CohereUnitCounts {
                    input_tokens: 9,
                    output_tokens: 4,
                }),
                tokens: Some(CohereUnitCounts {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
            }),
        };
        let out = translate_response(resp, "command-r-plus");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "command-r-plus");
        assert_eq!(
            out.choices[0]
                .message
                .content
                .as_ref()
                .map(|c| c.text().into_owned()),
            Some("Hello world".to_owned())
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
        // `tokens` wins over `billed_units`.
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.estimated, None);
    }

    #[test]
    fn usage_falls_back_to_billed_units_when_tokens_absent() {
        let usage = CohereUsage {
            billed_units: Some(CohereUnitCounts {
                input_tokens: 7,
                output_tokens: 3,
            }),
            tokens: None,
        };
        let out = usage_from_cohere(usage);
        assert_eq!(out.prompt_tokens, 7);
        assert_eq!(out.completion_tokens, 3);
    }

    #[test]
    fn missing_usage_object_yields_no_usage_for_the_gateway_to_estimate() {
        let resp = CohereChatResponse {
            id: "chat_2".to_owned(),
            finish_reason: Some("COMPLETE".to_owned()),
            message: CohereResponseMessage {
                content: vec![CohereContentBlock {
                    block_type: "text".to_owned(),
                    text: "hi".to_owned(),
                }],
                tool_calls: Vec::new(),
            },
            usage: None,
        };
        let out = translate_response(resp, "command-r-plus");
        assert!(out.usage.is_none());
    }

    #[test]
    fn tool_call_response_becomes_openai_tool_calls_with_null_content() {
        let resp = CohereChatResponse {
            id: "chat_3".to_owned(),
            finish_reason: Some("TOOL_CALL".to_owned()),
            message: CohereResponseMessage {
                content: Vec::new(),
                tool_calls: vec![json!({
                    "id": "call_9",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
                })],
            },
            usage: None,
        };
        let out = translate_response(resp, "command-r-plus");
        assert_eq!(out.choices[0].message.content, None);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        let calls = out.choices[0].message.extra["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls[0]["id"], "call_9");
    }

    #[test]
    fn n_greater_than_one_is_dropped_not_reflected_in_the_request() {
        let mut req = base_request(vec![msg("user", "hi")]);
        req.n = Some(3);
        // No `n` field exists on `CohereChatRequest` at all: this is a
        // compile-time guarantee that it cannot leak through, plus a
        // regression guard should the struct ever grow one.
        let value = serde_json::to_value(translate_request(&req, false)).unwrap();
        assert!(value.get("n").is_none());
    }
}
