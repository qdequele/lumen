//! Anthropic provider - chat completions with bidirectional translation.
//!
//! Anthropic's Messages API (`POST /v1/messages`) differs from OpenAI in
//! several ways this module bridges:
//!
//! * auth is `x-api-key` + `anthropic-version` headers, not a bearer token;
//! * `system` prompts are a top-level field, not a message with role `system`;
//! * `max_tokens` is REQUIRED (we default it when the client omits it);
//! * responses are `content` blocks with a `stop_reason` and
//!   `input_tokens`/`output_tokens` usage;
//! * streaming is typed SSE events, translated chunk by chunk in [`stream`]
//!   (bounded state - the response text is never accumulated).

mod stream;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{
    ChatChoice, ChatChunk, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ImageUrl,
    MessageContent, ProviderError, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt;
use tokio_util::sync::CancellationToken;

use self::stream::AnthropicTranslator;
use crate::chat::{items_to_chunks, items_to_sse_bytes, translate_sse_stream, StreamItem};
use crate::http::{open_stream_with_headers, post_json_with_headers};

/// Default Anthropic API base (no version suffix; the path adds `/v1/messages`).
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Anthropic API version header value pinned by this build.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens`; used when the client omits it.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// OpenAI chat fields the Messages API has no equivalent for (issues #72,
/// #91): no JSON mode / structured output, no sampling seed, no logprobs (nor
/// `top_logprobs`), no logit biasing, no frequency/presence penalties.
/// Rejected (strict) or dropped with a trace (lenient) before any upstream
/// call. `parallel_tool_calls` is NOT here: it maps to
/// `tool_choice.disable_parallel_tool_use` in [`translate_request`].
const UNSUPPORTED_CHAT_FIELDS: &[&str] = &[
    "response_format",
    "seed",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "frequency_penalty",
    "presence_penalty",
];

/// An Anthropic chat provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// API key sent as `x-api-key`. Redacted from `Debug`; never logged.
    api_key: Option<String>,
    /// When `true`, reject a request that sets an OpenAI field the Messages
    /// API cannot honor ([`UNSUPPORTED_CHAT_FIELDS`]) with a 400 (`LM-1001`)
    /// instead of silently dropping it (issue #72).
    strict: bool,
}

impl AnthropicProvider {
    /// Construct a provider. `base_url` defaults to the public Anthropic API.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        let base_url = base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Self {
            client,
            provider_name: provider_name.into(),
            base_url,
            api_key,
            strict: false,
        }
    }

    /// Set strict mode: reject (400, `LM-1001`) rather than drop request
    /// fields the Messages API cannot honor (issue #72). Defaults to `false`
    /// (lenient: drop with a `debug!` trace).
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

// ---- Wire types ----------------------------------------------------------

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    /// Only serialized on the streaming path.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    /// A plain string, or an array of content blocks (`tool_use`,
    /// `tool_result`) when the OpenAI message carried tool traffic.
    content: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: serde_json::Value,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
    // `tool_use` block fields.
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

// Field names mirror Anthropic's `usage` JSON keys (serde-bound), so the
// shared `_tokens` postfix is intentional, not a naming smell.
#[allow(clippy::struct_field_names)]
#[derive(Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    /// Prompt tokens written to the cache on this request (cache *write*).
    /// Absent unless the request used prompt caching; `None`, never zero, so
    /// the breakdown is only surfaced when the upstream reported it (issue #99).
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    /// Prompt tokens served from the cache (cache *read*).
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

/// Build the OpenAI-compatible prompt-token breakdown from Anthropic's cache
/// counts, mapping `cache_read_input_tokens` to `cached_tokens` (same read/hit
/// semantics) and keeping `cache_creation_input_tokens` in the distinct
/// `cache_creation_tokens` slot (a write, with no OpenAI equivalent). Returns
/// `None` when neither is reported, so absence stays `None` (ADR 003).
fn anthropic_prompt_details(usage: &AnthropicUsage) -> Option<lumen_core::PromptTokensDetails> {
    if usage.cache_read_input_tokens.is_none() && usage.cache_creation_input_tokens.is_none() {
        return None;
    }
    Some(lumen_core::PromptTokensDetails {
        cached_tokens: usage.cache_read_input_tokens,
        cache_creation_tokens: usage.cache_creation_input_tokens,
    })
}

/// Translate an Anthropic `stop_reason` to an OpenAI `finish_reason`.
fn map_finish_reason(stop_reason: Option<&str>) -> Option<String> {
    match stop_reason {
        Some("end_turn" | "stop_sequence") => Some("stop".to_owned()),
        Some("max_tokens") => Some("length".to_owned()),
        Some("tool_use") => Some("tool_calls".to_owned()),
        Some(other) => Some(other.to_owned()),
        None => None,
    }
}

/// Build the Anthropic request body from an OpenAI-shaped [`ChatRequest`].
/// `stream` is set explicitly by the calling path, never taken from the
/// client's request (the gateway decides which upstream mode it needs).
fn translate_request(req: &ChatRequest, stream: bool) -> AnthropicRequest {
    // System messages are hoisted into the top-level `system` field, joined by
    // blank lines; every other message keeps its role. Tool traffic maps to
    // content blocks: assistant `tool_calls` → `tool_use`, role `tool` →
    // `tool_result` (consecutive tool results merge into one user message,
    // matching Anthropic's role-alternation expectations).
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    for m in &req.messages {
        let text = m
            .content
            .as_ref()
            .map(|c| c.text().into_owned())
            .unwrap_or_default();
        match m.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.extra.get("tool_call_id").cloned().unwrap_or_default(),
                    "content": text,
                });
                match messages.last_mut() {
                    // Merge into the previous tool-result user message.
                    Some(prev) if prev.role == "user" && prev.content.is_array() => {
                        if let Some(blocks) = prev.content.as_array_mut() {
                            blocks.push(block);
                        }
                    }
                    _ => messages.push(AnthropicMessage {
                        role: "user".to_owned(),
                        content: serde_json::Value::Array(vec![block]),
                    }),
                }
            }
            "assistant"
                if m.extra
                    .get("tool_calls")
                    .is_some_and(serde_json::Value::is_array) =>
            {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if let Some(calls) = m.extra.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").cloned().unwrap_or_default(),
                            "name": call.pointer("/function/name").cloned().unwrap_or_default(),
                            "input": parse_tool_arguments(call.pointer("/function/arguments")),
                        }));
                    }
                }
                messages.push(AnthropicMessage {
                    role: "assistant".to_owned(),
                    content: serde_json::Value::Array(blocks),
                });
            }
            role => messages.push(AnthropicMessage {
                role: role.to_owned(),
                content: anthropic_content(m.content.as_ref(), &text),
            }),
        }
    }

    let stop_sequences = req
        .stop
        .as_ref()
        .map(collect_stop_sequences)
        .unwrap_or_default();

    let tools = translate_tools(req);
    let mut tool_choice = req.extra.get("tool_choice").and_then(translate_tool_choice);
    // OpenAI `parallel_tool_calls: false` maps to Anthropic's
    // `tool_choice.disable_parallel_tool_use: true` (issue #72). `true` is the
    // upstream default on both sides, so it needs no wire field. Only
    // meaningful alongside tools, and never on `{"type": "none"}` (no tool may
    // be called at all there).
    if req
        .extra
        .get("parallel_tool_calls")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
        && !tools.is_empty()
    {
        let choice = tool_choice.get_or_insert_with(|| json!({ "type": "auto" }));
        if choice.get("type").and_then(|v| v.as_str()) != Some("none") {
            if let Some(obj) = choice.as_object_mut() {
                obj.insert("disable_parallel_tool_use".to_owned(), json!(true));
            }
        }
    }

    AnthropicRequest {
        model: req.model.clone(),
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        messages,
        system: if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        },
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences,
        tools,
        tool_choice,
        stream,
    }
}

/// Build an Anthropic message `content`: a plain string when there are no
/// images, else an array of `text`/`image` blocks (order preserved).
fn anthropic_content(content: Option<&MessageContent>, text: &str) -> serde_json::Value {
    match content {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    blocks.push(anthropic_image_block(img));
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
            }
            serde_json::Value::Array(blocks)
        }
        // No images (string, text-only parts, or none): a plain string.
        _ => serde_json::Value::String(text.to_owned()),
    }
}

/// Translate one OpenAI `image_url` into an Anthropic image source block.
/// An `anthropic-file:<file_id>` reference (issue #12) becomes a `file`
/// source pointing at a pre-uploaded Files API object; `data:` URIs become a
/// `base64` source; remote URLs a `url` source (Anthropic fetches it). The
/// gateway never fetches the URL itself. A mismatched provider-native
/// reference (e.g. a Gemini `gs://` URI reaching this path via a fallback
/// chain) falls through to the `url` source, same as any other opaque
/// string - the resolved primary's pre-flight (`LM-2008`) is what makes the
/// common case an honest 400 rather than a confusing upstream error.
fn anthropic_image_block(image: &ImageUrl) -> serde_json::Value {
    if let Some(file_id) = image.anthropic_file_id() {
        json!({
            "type": "image",
            "source": { "type": "file", "file_id": file_id },
        })
    } else if let Some(data) = image.as_data_uri() {
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": data.media_type, "data": data.base64_data },
        })
    } else {
        json!({
            "type": "image",
            "source": { "type": "url", "url": image.url },
        })
    }
}

/// OpenAI tool-call `arguments` is a JSON *string*; Anthropic `input` is the
/// object itself. Unparseable arguments degrade to an empty object.
fn parse_tool_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    arguments
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}))
}

/// OpenAI `tools` (`{type: "function", function: {name, description,
/// parameters}}`) → Anthropic `tools` (`{name, description, input_schema}`).
/// Non-function entries are skipped.
fn translate_tools(req: &ChatRequest) -> Vec<AnthropicTool> {
    let Some(tools) = req.extra.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(AnthropicTool {
                name: function.get("name")?.as_str()?.to_owned(),
                description: function
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                input_schema: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            })
        })
        .collect()
}

/// OpenAI `tool_choice` → Anthropic `tool_choice`. Unknown shapes are dropped
/// (the upstream default, `auto`, applies) rather than guessed.
fn translate_tool_choice(choice: &serde_json::Value) -> Option<serde_json::Value> {
    match choice {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" => Some(json!({ "type": "any" })),
            "none" => Some(json!({ "type": "none" })),
            _ => None,
        },
        serde_json::Value::Object(_) => choice
            .pointer("/function/name")
            .and_then(|v| v.as_str())
            .map(|name| json!({ "type": "tool", "name": name })),
        _ => None,
    }
}

/// OpenAI `stop` is a string or array of strings; normalise to a list.
fn collect_stop_sequences(stop: &serde_json::Value) -> Vec<String> {
    match stop {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Build an OpenAI-shaped [`ChatResponse`] from an Anthropic response.
fn translate_response(resp: AnthropicResponse, requested_model: &str) -> ChatResponse {
    let mut content = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    for block in resp.content {
        match block.block_type.as_str() {
            "text" => content.push_str(&block.text),
            "tool_use" => tool_calls.push(json!({
                "id": block.id,
                "type": "function",
                "function": {
                    "name": block.name,
                    // OpenAI carries arguments as a JSON string.
                    "arguments": block.input.unwrap_or_else(|| json!({})).to_string(),
                },
            })),
            _ => {}
        }
    }

    let mut extra = serde_json::Map::new();
    if !tool_calls.is_empty() {
        extra.insert(
            "tool_calls".to_owned(),
            serde_json::Value::Array(tool_calls),
        );
    }
    // OpenAI uses `content: null` for pure tool-call messages.
    let content = if content.is_empty() && !extra.is_empty() {
        None
    } else {
        Some(MessageContent::Text(content))
    };

    let model = if resp.model.is_empty() {
        requested_model.to_owned()
    } else {
        resp.model
    };

    let usage = Usage {
        prompt_tokens: resp.usage.input_tokens,
        completion_tokens: resp.usage.output_tokens,
        total_tokens: resp
            .usage
            .input_tokens
            .saturating_add(resp.usage.output_tokens),
        estimated: None,
        prompt_tokens_details: anthropic_prompt_details(&resp.usage),
        // Anthropic does not report a reasoning-token split.
        completion_tokens_details: None,
    };

    ChatResponse {
        id: resp.id,
        object: "chat.completion".to_owned(),
        created: crate::mapping::unix_timestamp(), // Anthropic returns no creation time.
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_owned(),
                content,
                name: None,
                extra,
            },
            finish_reason: map_finish_reason(resp.stop_reason.as_deref()),
        }],
        usage: Some(usage),
        extra: serde_json::Map::new(),
    }
}

/// Fuzz-only shims over the private translation functions above.
///
/// `cargo fuzz` builds the whole dependency graph (including this crate)
/// with `--cfg fuzzing` set, so these functions compile only under
/// `cargo +nightly fuzz run ...` and add nothing to normal builds (`cargo
/// build`/`clippy`/`test` never set this cfg). This is the least invasive
/// way to reach `translate_request`/`translate_response`: no visibility
/// changes to the wire types (`AnthropicRequest`/`AnthropicResponse` stay
/// private), no new public API surface outside fuzzing builds.
#[cfg(fuzzing)]
pub mod fuzzing {
    use super::{translate_request, translate_response, AnthropicResponse};
    use lumen_core::ChatRequest;

    /// Translate an arbitrary `ChatRequest` and serialize the result; must
    /// never panic regardless of message shape, tool traffic, or `extra`.
    pub fn fuzz_translate_request(req: &ChatRequest, stream: bool) {
        let translated = translate_request(req, stream);
        let _ = serde_json::to_vec(&translated);
    }

    /// Deserialize arbitrary bytes as an Anthropic response and translate
    /// whatever parses; must never panic on malformed or adversarial input.
    pub fn fuzz_translate_response(data: &[u8], requested_model: &str) {
        if let Ok(resp) = serde_json::from_slice::<AnthropicResponse>(data) {
            let _ = translate_response(resp, requested_model);
        }
    }
}

impl AnthropicProvider {
    /// Open the upstream stream and translate its events (shared by both
    /// streaming trait methods).
    async fn open_translated_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, ProviderError>>, ProviderError> {
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        let url = format!("{}/v1/messages", self.base_url);
        let body = translate_request(&req, true);
        let headers = [
            ("x-api-key", self.api_key.as_deref().unwrap_or("")),
            ("anthropic-version", ANTHROPIC_VERSION),
        ];

        let bytes = open_stream_with_headers(
            &self.client,
            &url,
            &body,
            &headers,
            &self.provider_name,
            &cancel,
        )
        .await?;
        Ok(translate_sse_stream(
            bytes,
            AnthropicTranslator::new(&req.model),
        ))
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        let url = format!("{}/v1/messages", self.base_url);
        let body = translate_request(&req, false);
        let headers = [
            ("x-api-key", self.api_key.as_deref().unwrap_or("")),
            ("anthropic-version", ANTHROPIC_VERSION),
        ];

        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &body,
            &headers,
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: AnthropicResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("anthropic response: {e}")))?;
        Ok(translate_response(parsed, &req.model))
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_chunks(items))
    }

    /// Event-by-event translation to OpenAI SSE frames. `data: [DONE]` is
    /// emitted only on a genuine upstream `message_stop`, so a mid-stream
    /// upstream death surfaces as a missing terminator (LM-3010 downstream).
    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_sse_bytes(items))
    }

    /// Anthropic is the only provider that can resolve its own Files API
    /// `file_id` references (issue #12); a mismatch is caught pre-flight
    /// with `LM-2008`.
    fn accepts_anthropic_file_id(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: Some(MessageContent::Text(content.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn text_block(text: &str) -> AnthropicContentBlock {
        AnthropicContentBlock {
            block_type: "text".to_owned(),
            text: text.to_owned(),
            id: String::new(),
            name: String::new(),
            input: None,
        }
    }

    #[test]
    fn request_hoists_system_and_defaults_max_tokens() {
        let req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![
                msg("system", "be brief"),
                msg("user", "hi"),
                msg("system", "also polite"),
            ],
            temperature: Some(0.5),
            top_p: None,
            max_tokens: None,
            n: None,
            stop: Some(json!(["STOP"])),
            stream: false,
            extra: serde_json::Map::new(),
        };
        let out = translate_request(&req, false);
        assert_eq!(out.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(!out.stream);
        assert_eq!(out.system.as_deref(), Some("be brief\n\nalso polite"));
        // Only the non-system message survives in `messages`.
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.stop_sequences, vec!["STOP".to_owned()]);
        assert_eq!(out.temperature, Some(0.5));
    }

    #[test]
    fn response_concatenates_text_blocks_and_maps_stop_reason() {
        let resp = AnthropicResponse {
            id: "msg_1".to_owned(),
            content: vec![text_block("Hello "), text_block("world")],
            model: "claude-x".to_owned(),
            stop_reason: Some("max_tokens".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        };
        let out = translate_response(resp, "claude-x");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(
            out.choices[0]
                .message
                .content
                .as_ref()
                .map(|c| c.text().into_owned()),
            Some("Hello world".to_owned())
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        // No cache fields upstream: the breakdown stays absent (issue #99).
        assert!(usage.prompt_tokens_details.is_none());
        assert_eq!(usage.cached_tokens(), None);
        assert_eq!(usage.cache_write_tokens(), None);
    }

    /// Issue #99: Anthropic's `cache_read_input_tokens` maps to the
    /// OpenAI-compatible `cached_tokens`, and `cache_creation_input_tokens`
    /// stays in the distinct `cache_creation_tokens` (write) slot.
    #[test]
    fn response_surfaces_anthropic_cache_token_breakdown() {
        let resp = AnthropicResponse {
            id: "msg_c".to_owned(),
            content: vec![text_block("hi")],
            model: "claude-x".to_owned(),
            stop_reason: Some("end_turn".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 12,
                output_tokens: 3,
                cache_creation_input_tokens: Some(20),
                cache_read_input_tokens: Some(80),
            },
        };
        let out = translate_response(resp, "claude-x");
        let usage = out.usage.unwrap();
        assert_eq!(usage.cached_tokens(), Some(80));
        assert_eq!(usage.cache_write_tokens(), Some(20));
        // Serializes in the OpenAI-compatible nested shape.
        let body = serde_json::to_value(usage).unwrap();
        assert_eq!(body["prompt_tokens_details"]["cached_tokens"], 80);
        assert_eq!(body["prompt_tokens_details"]["cache_creation_tokens"], 20);
    }

    /// A response reporting only a cache *read* (no write) surfaces just
    /// `cached_tokens`, leaving the write slot absent.
    #[test]
    fn response_with_only_cache_read_omits_the_write_slot() {
        let resp = AnthropicResponse {
            id: "msg_r".to_owned(),
            content: vec![text_block("hi")],
            model: "claude-x".to_owned(),
            stop_reason: Some("end_turn".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 5,
                output_tokens: 1,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(64),
            },
        };
        let out = translate_response(resp, "claude-x");
        let usage = out.usage.unwrap();
        assert_eq!(usage.cached_tokens(), Some(64));
        assert_eq!(usage.cache_write_tokens(), None);
        let body = serde_json::to_value(usage).unwrap();
        assert_eq!(body["prompt_tokens_details"]["cached_tokens"], 64);
        assert!(body["prompt_tokens_details"]
            .get("cache_creation_tokens")
            .is_none());
    }

    /// Anthropic responses carry no creation time; the gateway must stamp a
    /// real unix timestamp instead of the historical hardcoded `0`.
    #[test]
    fn response_created_is_a_real_unix_timestamp_not_zero() {
        let resp = AnthropicResponse {
            id: "msg_1".to_owned(),
            content: vec![text_block("hi")],
            model: "claude-x".to_owned(),
            stop_reason: Some("end_turn".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        };
        let out = translate_response(resp, "claude-x");
        // Sanity-bound against a fixed past instant (2024-01-01 UTC) rather
        // than pinning an exact wall-clock value.
        assert!(out.created > 1_704_067_200);
    }

    /// Criterion 3: an OpenAI request WITH tools translates to the exact
    /// expected Anthropic JSON (request side).
    #[test]
    fn request_with_tools_matches_expected_anthropic_json_exactly() {
        let mut assistant_extra = serde_json::Map::new();
        assistant_extra.insert(
            "tool_calls".to_owned(),
            json!([{
                "id": "call_1",
                "type": "function",
                "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
            }]),
        );
        let mut tool_extra = serde_json::Map::new();
        tool_extra.insert("tool_call_id".to_owned(), json!("call_1"));

        let mut extra = serde_json::Map::new();
        extra.insert(
            "tools".to_owned(),
            json!([{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Weather lookup",
                    "parameters": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"]
                    }
                }
            }]),
        );
        extra.insert("tool_choice".to_owned(), json!("auto"));

        let req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![
                msg("system", "be brief"),
                msg("user", "weather in Paris?"),
                ChatMessage {
                    role: "assistant".to_owned(),
                    content: None,
                    name: None,
                    extra: assistant_extra,
                },
                ChatMessage {
                    role: "tool".to_owned(),
                    content: Some(MessageContent::Text("18°C, sunny".to_owned())),
                    name: None,
                    extra: tool_extra,
                },
            ],
            temperature: None,
            top_p: None,
            max_tokens: Some(1024),
            n: None,
            stop: None,
            stream: false,
            extra,
        };

        let out = serde_json::to_value(translate_request(&req, false)).unwrap();
        assert_eq!(
            out,
            json!({
                "model": "claude-x",
                "max_tokens": 1024,
                "system": "be brief",
                "messages": [
                    { "role": "user", "content": "weather in Paris?" },
                    { "role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": "call_1",
                        "name": "get_weather",
                        "input": { "city": "Paris" }
                    }] },
                    { "role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call_1",
                        "content": "18°C, sunny"
                    }] }
                ],
                "tools": [{
                    "name": "get_weather",
                    "description": "Weather lookup",
                    "input_schema": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"]
                    }
                }],
                "tool_choice": { "type": "auto" }
            })
        );
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let tool_msg = |id: &str, text: &str| {
            let mut extra = serde_json::Map::new();
            extra.insert("tool_call_id".to_owned(), json!(id));
            ChatMessage {
                role: "tool".to_owned(),
                content: Some(MessageContent::Text(text.to_owned())),
                name: None,
                extra,
            }
        };
        let req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![
                msg("user", "two lookups please"),
                tool_msg("call_1", "a"),
                tool_msg("call_2", "b"),
            ],
            temperature: None,
            top_p: None,
            max_tokens: Some(64),
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let out = translate_request(&req, false);
        // user text + ONE merged tool-result user message with two blocks.
        assert_eq!(out.messages.len(), 2);
        let blocks = out.messages[1].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "call_1");
        assert_eq!(blocks[1]["tool_use_id"], "call_2");
    }

    #[test]
    fn tool_choice_variants_map_to_anthropic_shapes() {
        assert_eq!(
            translate_tool_choice(&json!("required")),
            Some(json!({ "type": "any" }))
        );
        assert_eq!(
            translate_tool_choice(&json!("none")),
            Some(json!({ "type": "none" }))
        );
        assert_eq!(
            translate_tool_choice(&json!({
                "type": "function", "function": { "name": "f" }
            })),
            Some(json!({ "type": "tool", "name": "f" }))
        );
        // Unknown shapes are dropped, not guessed.
        assert_eq!(translate_tool_choice(&json!(42)), None);
    }

    /// Criterion 3 (response side): `tool_use` blocks come back as OpenAI
    /// `tool_calls`, with `arguments` re-encoded as a JSON string.
    #[test]
    fn response_tool_use_blocks_become_openai_tool_calls() {
        let resp = AnthropicResponse {
            id: "msg_2".to_owned(),
            content: vec![AnthropicContentBlock {
                block_type: "tool_use".to_owned(),
                text: String::new(),
                id: "toolu_7".to_owned(),
                name: "get_weather".to_owned(),
                input: Some(json!({ "city": "Paris" })),
            }],
            model: "claude-x".to_owned(),
            stop_reason: Some("tool_use".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 4,
                output_tokens: 2,
                ..Default::default()
            },
        };
        let out = translate_response(resp, "claude-x");
        let message = &out.choices[0].message;
        // Pure tool-call message: content is null, tool_calls carry the call.
        assert_eq!(message.content, None);
        let calls = message.extra["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "toolu_7");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!({ "city": "Paris" }).to_string()
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn finish_reason_mapping_covers_known_values() {
        assert_eq!(map_finish_reason(Some("end_turn")).as_deref(), Some("stop"));
        assert_eq!(
            map_finish_reason(Some("tool_use")).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(map_finish_reason(None), None);
    }

    #[test]
    fn data_uri_image_becomes_a_base64_source_block() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "claude".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![
                    ContentPart {
                        kind: "text".to_owned(),
                        text: Some("describe".to_owned()),
                        image_url: None,
                        extra: serde_json::Map::new(),
                    },
                    ContentPart {
                        kind: "image_url".to_owned(),
                        text: None,
                        image_url: Some(ImageUrl {
                            url: "data:image/png;base64,AAAA".to_owned(),
                            detail: None,
                        }),
                        extra: serde_json::Map::new(),
                    },
                ])),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let body = serde_json::to_value(translate_request(&req, false)).unwrap();
        let blocks = &body["messages"][0]["content"];
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "describe");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn remote_url_image_becomes_a_url_source_block() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "claude".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![ContentPart {
                    kind: "image_url".to_owned(),
                    text: None,
                    image_url: Some(ImageUrl {
                        url: "https://ex.com/c.png".to_owned(),
                        detail: None,
                    }),
                    extra: serde_json::Map::new(),
                }])),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let body = serde_json::to_value(translate_request(&req, false)).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://ex.com/c.png");
    }

    /// Issue #12: an `anthropic-file:<file_id>` reference becomes a `file`
    /// source block pointing at the pre-uploaded Files API object, not a
    /// `url`/`base64` source.
    #[test]
    fn anthropic_file_id_becomes_a_file_source_block() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "claude".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![ContentPart {
                    kind: "image_url".to_owned(),
                    text: None,
                    image_url: Some(ImageUrl {
                        url: "anthropic-file:file_011CNvxvfvyGnGnDtjPtzY9J".to_owned(),
                        detail: None,
                    }),
                    extra: serde_json::Map::new(),
                }])),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let body = serde_json::to_value(translate_request(&req, false)).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "file");
        assert_eq!(block["source"]["file_id"], "file_011CNvxvfvyGnGnDtjPtzY9J");
        assert!(block["source"].get("data").is_none());
        assert!(block["source"].get("url").is_none());
    }

    fn tools_extra() -> serde_json::Map<String, serde_json::Value> {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tools".to_owned(),
            json!([{
                "type": "function",
                "function": { "name": "get_weather", "parameters": { "type": "object" } }
            }]),
        );
        extra
    }

    /// Issue #72: OpenAI `parallel_tool_calls: false` maps to Anthropic's
    /// `tool_choice.disable_parallel_tool_use: true`, defaulting the choice to
    /// `auto` when the client sent none.
    #[test]
    fn parallel_tool_calls_false_maps_to_disable_parallel_tool_use() {
        let mut extra = tools_extra();
        extra.insert("parallel_tool_calls".to_owned(), json!(false));
        let mut req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![msg("user", "hi")],
            temperature: None,
            top_p: None,
            max_tokens: Some(64),
            n: None,
            stop: None,
            stream: false,
            extra,
        };
        let out = translate_request(&req, false);
        assert_eq!(
            out.tool_choice,
            Some(json!({ "type": "auto", "disable_parallel_tool_use": true }))
        );

        // With an explicit tool_choice, the flag merges into it.
        req.extra
            .insert("tool_choice".to_owned(), json!("required"));
        let out = translate_request(&req, false);
        assert_eq!(
            out.tool_choice,
            Some(json!({ "type": "any", "disable_parallel_tool_use": true }))
        );
    }

    /// `parallel_tool_calls: true` is the default on both sides; without
    /// tools the flag is meaningless. Neither emits a `tool_choice`.
    #[test]
    fn parallel_tool_calls_true_or_toolless_emits_no_tool_choice() {
        let mut extra = tools_extra();
        extra.insert("parallel_tool_calls".to_owned(), json!(true));
        let mut req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![msg("user", "hi")],
            temperature: None,
            top_p: None,
            max_tokens: Some(64),
            n: None,
            stop: None,
            stream: false,
            extra,
        };
        assert_eq!(translate_request(&req, false).tool_choice, None);

        req.extra = serde_json::Map::new();
        req.extra
            .insert("parallel_tool_calls".to_owned(), json!(false));
        assert_eq!(translate_request(&req, false).tool_choice, None);
    }

    /// Issue #72: in strict mode, a field the Messages API cannot honor is an
    /// honest client rejection (`UnsupportedField` -> 400, LM-1001) BEFORE any
    /// upstream call - the base URL below is unroutable on purpose.
    #[tokio::test]
    async fn strict_mode_rejects_response_format_seed_and_logprobs() {
        use tokio_util::sync::CancellationToken;
        let provider = AnthropicProvider::new(
            reqwest::Client::new(),
            "anthropic-test",
            Some("http://127.0.0.1:1".to_owned()),
            Some("sk-ant-test".to_owned()),
        )
        .with_strict(true);

        for (field, value) in [
            ("response_format", json!({ "type": "json_object" })),
            ("seed", json!(42)),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(5)),
            ("logit_bias", json!({ "50256": -100 })),
            ("frequency_penalty", json!(0.5)),
            ("presence_penalty", json!(0.25)),
        ] {
            let mut extra = serde_json::Map::new();
            extra.insert(field.to_owned(), value);
            let req = ChatRequest {
                model: "claude-x".to_owned(),
                messages: vec![msg("user", "hi")],
                temperature: None,
                top_p: None,
                max_tokens: Some(16),
                n: None,
                stop: None,
                stream: false,
                extra,
            };
            let err = provider
                .chat(req.clone(), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                matches!(
                    &err,
                    ProviderError::UnsupportedField { provider, field: f }
                        if provider == "anthropic-test" && f == field
                ),
                "expected UnsupportedField for {field}, got {err:?}"
            );
            // The streaming path enforces the same pre-flight.
            let err = provider
                .chat_stream(req, CancellationToken::new())
                .await
                .err()
                .expect("stream must be rejected too");
            assert!(matches!(err, ProviderError::UnsupportedField { .. }));
        }
    }

    /// Lenient (default) mode drops the fields: the wire body simply never
    /// carries them (compile-time: the struct has no such fields) and the
    /// request is not rejected up front.
    #[test]
    fn lenient_translation_emits_no_unsupported_fields_on_the_wire() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "response_format".to_owned(),
            json!({ "type": "json_object" }),
        );
        extra.insert("seed".to_owned(), json!(42));
        extra.insert("logprobs".to_owned(), json!(true));
        let req = ChatRequest {
            model: "claude-x".to_owned(),
            messages: vec![msg("user", "hi")],
            temperature: None,
            top_p: None,
            max_tokens: Some(16),
            n: None,
            stop: None,
            stream: false,
            extra,
        };
        let body = serde_json::to_value(translate_request(&req, false)).unwrap();
        for field in ["response_format", "seed", "logprobs"] {
            assert!(body.get(field).is_none(), "{field} must not reach the wire");
        }
    }

    #[test]
    fn anthropic_provider_accepts_its_own_file_id() {
        let provider = AnthropicProvider::new(
            reqwest::Client::new(),
            "anthropic".to_owned(),
            None,
            Some("sk-ant-test".to_owned()),
        );
        assert!(provider.accepts_anthropic_file_id());
        assert!(!provider.accepts_gemini_file_uri());
    }

    #[test]
    fn text_only_message_stays_a_plain_string() {
        use lumen_core::{ChatMessage, ChatRequest, MessageContent};
        let req = ChatRequest {
            model: "claude".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Text("hello".to_owned())),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let body = serde_json::to_value(translate_request(&req, false)).unwrap();
        assert_eq!(body["messages"][0]["content"], "hello");
    }
}
