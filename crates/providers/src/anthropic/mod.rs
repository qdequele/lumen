//! Anthropic provider — chat completions with bidirectional translation.
//!
//! Anthropic's Messages API (`POST /v1/messages`) differs from OpenAI in
//! several ways this module bridges (non-streaming in this slice; streaming
//! event translation and full tool mapping land in the M4 streaming slice):
//!
//! * auth is `x-api-key` + `anthropic-version` headers, not a bearer token;
//! * `system` prompts are a top-level field, not a message with role `system`;
//! * `max_tokens` is REQUIRED (we default it when the client omits it);
//! * responses are `content` blocks with a `stop_reason` and
//!   `input_tokens`/`output_tokens` usage.

use async_trait::async_trait;
use ferrogate_core::{
    ChatChoice, ChatChunk, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ProviderError,
    Usage,
};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::chat::single_shot_stream;
use crate::http::post_json_with_headers;

/// Default Anthropic API base (no version suffix; the path adds `/v1/messages`).
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Anthropic API version header value pinned by this build.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens`; used when the client omits it.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// An Anthropic chat provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// API key sent as `x-api-key`. Redacted from `Debug`; never logged.
    api_key: Option<String>,
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
        }
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
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
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
}

#[derive(Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
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
fn translate_request(req: &ChatRequest) -> AnthropicRequest {
    // System messages are hoisted into the top-level `system` field, joined by
    // blank lines; every other message keeps its role.
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    for m in &req.messages {
        let text = m.content.clone().unwrap_or_default();
        if m.role == "system" {
            if !text.is_empty() {
                system_parts.push(text);
            }
        } else {
            messages.push(AnthropicMessage {
                role: m.role.clone(),
                content: text,
            });
        }
    }

    let stop_sequences = req
        .stop
        .as_ref()
        .map(collect_stop_sequences)
        .unwrap_or_default();

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
    let content: String = resp
        .content
        .into_iter()
        .filter(|b| b.block_type == "text")
        .map(|b| b.text)
        .collect();

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
    };

    ChatResponse {
        id: resp.id,
        object: "chat.completion".to_owned(),
        created: 0, // Anthropic does not return a creation timestamp.
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_owned(),
                content: Some(content),
                name: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: map_finish_reason(resp.stop_reason.as_deref()),
        }],
        usage: Some(usage),
        extra: serde_json::Map::new(),
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = translate_request(&req);
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
        // Interim single-shot; real Anthropic SSE event translation is Slice 2.
        let resp = self.chat(req, cancel).await?;
        Ok(single_shot_stream(resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: Some(content.to_owned()),
            name: None,
            extra: serde_json::Map::new(),
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
        let out = translate_request(&req);
        assert_eq!(out.max_tokens, DEFAULT_MAX_TOKENS);
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
            content: vec![
                AnthropicContentBlock {
                    block_type: "text".to_owned(),
                    text: "Hello ".to_owned(),
                },
                AnthropicContentBlock {
                    block_type: "text".to_owned(),
                    text: "world".to_owned(),
                },
            ],
            model: "claude-x".to_owned(),
            stop_reason: Some("max_tokens".to_owned()),
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };
        let out = translate_response(resp, "claude-x");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(
            out.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
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
}
