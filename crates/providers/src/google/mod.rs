//! Google Gemini provider — chat completions with bidirectional translation.
//!
//! Gemini's `generateContent` API differs from OpenAI in several ways this
//! module bridges:
//!
//! * auth is an `x-goog-api-key` header (the key is never put in the URL);
//! * the model is part of the URL path (`/models/{model}:generateContent`,
//!   or `:streamGenerateContent?alt=sse` when streaming);
//! * messages are `contents` with roles `user`/`model` (assistant → `model`);
//!   system prompts go in a top-level `systemInstruction`;
//! * generation params live under `generationConfig`;
//! * responses are `candidates` with a `finishReason` and `usageMetadata`;
//! * streaming events are partial responses, translated fragment by fragment
//!   in [`stream`] (bounded state — the text is never accumulated).

mod stream;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{
    ChatChoice, ChatChunk, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ProviderError,
    Usage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use self::stream::GoogleTranslator;
use crate::chat::{items_to_chunks, items_to_sse_bytes, translate_sse_stream, StreamItem};
use crate::http::{open_stream_with_headers, post_json_with_headers};

/// Default Gemini API base (the path adds `/v1beta/models/...`).
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// A Google Gemini chat provider.
pub struct GoogleProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// API key sent as `x-goog-api-key`. Redacted from `Debug`; never logged,
    /// and never placed in the URL.
    api_key: Option<String>,
}

impl GoogleProvider {
    /// Construct a provider. `base_url` defaults to the public Gemini API.
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
impl fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

// ---- Wire types ----------------------------------------------------------

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystem>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiSystem {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(rename = "stopSequences", skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: GeminiUsage,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: GeminiResponseContent,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    #[serde(default)]
    text: String,
}

#[derive(Default, Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates: u32,
    #[serde(rename = "totalTokenCount", default)]
    total: u32,
}

/// Translate a Gemini `finishReason` to an OpenAI `finish_reason`.
fn map_finish_reason(reason: Option<&str>) -> Option<String> {
    match reason {
        Some("MAX_TOKENS") => Some("length".to_owned()),
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            Some("content_filter".to_owned())
        }
        // "STOP" and any unrecognised reason map to the default stop.
        Some(_) => Some("stop".to_owned()),
        None => None,
    }
}

/// Build the Gemini request body from an OpenAI-shaped [`ChatRequest`].
fn translate_request(req: &ChatRequest) -> GeminiRequest {
    let mut system_parts: Vec<GeminiPart> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();
    for m in &req.messages {
        let text = m.content.clone().unwrap_or_default();
        match m.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system_parts.push(GeminiPart { text });
                }
            }
            // OpenAI's `assistant` is Gemini's `model`; everything else → user.
            role => contents.push(GeminiContent {
                role: if role == "assistant" {
                    "model".to_owned()
                } else {
                    "user".to_owned()
                },
                parts: vec![GeminiPart { text }],
            }),
        }
    }

    let stop_sequences = req
        .stop
        .as_ref()
        .map(collect_stop_sequences)
        .unwrap_or_default();
    let generation_config = GeminiGenerationConfig {
        max_output_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences,
    };

    GeminiRequest {
        contents,
        system_instruction: if system_parts.is_empty() {
            None
        } else {
            Some(GeminiSystem {
                parts: system_parts,
            })
        },
        generation_config: Some(generation_config),
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

/// Build an OpenAI-shaped [`ChatResponse`] from a Gemini response.
fn translate_response(resp: GeminiResponse, requested_model: &str) -> ChatResponse {
    let candidate = resp.candidates.into_iter().next();
    let (content, finish_reason) = candidate
        .map(|c| {
            let text: String = c.content.parts.into_iter().map(|p| p.text).collect();
            (text, map_finish_reason(c.finish_reason.as_deref()))
        })
        .unwrap_or_default();

    let usage = Usage {
        prompt_tokens: resp.usage_metadata.prompt,
        completion_tokens: resp.usage_metadata.candidates,
        total_tokens: resp.usage_metadata.total,
        estimated: None,
    };

    ChatResponse {
        id: String::new(),
        object: "chat.completion".to_owned(),
        created: 0, // Gemini does not return a creation timestamp.
        model: requested_model.to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_owned(),
                content: Some(content),
                name: None,
                extra: serde_json::Map::new(),
            },
            finish_reason,
        }],
        usage: Some(usage),
        extra: serde_json::Map::new(),
    }
}

#[async_trait]
impl ChatProvider for GoogleProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        // The model is part of the path; the key is a header, never the URL.
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, req.model
        );
        let body = translate_request(&req);
        let headers = [("x-goog-api-key", self.api_key.as_deref().unwrap_or(""))];

        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &body,
            &headers,
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: GeminiResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("google gemini response: {e}")))?;
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

    /// Fragment-by-fragment translation to OpenAI SSE frames. `data: [DONE]`
    /// is emitted only after a genuine upstream `finishReason`, so a mid-stream
    /// upstream death surfaces as a missing terminator (LM-3010 downstream).
    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_sse_bytes(items))
    }
}

impl GoogleProvider {
    /// Open the upstream SSE stream and translate its fragments (shared by
    /// both streaming trait methods).
    async fn open_translated_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, ProviderError>>, ProviderError> {
        // `alt=sse` selects SSE framing; the key stays in a header, never the URL.
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
        let body = translate_request(&req);
        let headers = [("x-goog-api-key", self.api_key.as_deref().unwrap_or(""))];

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
            GoogleTranslator::new(&req.model),
        ))
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

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "gemini-2.0".to_owned(),
            messages,
            temperature: Some(0.3),
            top_p: None,
            max_tokens: Some(256),
            n: None,
            stop: Some(json!("END")),
            stream: false,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn request_maps_roles_and_hoists_system() {
        let out = translate_request(&request(vec![
            msg("system", "be brief"),
            msg("user", "hi"),
            msg("assistant", "hello"),
            msg("user", "more"),
        ]));
        assert_eq!(
            out.system_instruction.as_ref().unwrap().parts[0].text,
            "be brief"
        );
        assert_eq!(out.contents.len(), 3);
        assert_eq!(out.contents[0].role, "user");
        // OpenAI assistant → Gemini model.
        assert_eq!(out.contents[1].role, "model");
        assert_eq!(out.contents[2].role, "user");
        let cfg = out.generation_config.unwrap();
        assert_eq!(cfg.max_output_tokens, Some(256));
        assert_eq!(cfg.stop_sequences, vec!["END".to_owned()]);
    }

    #[test]
    fn response_concatenates_parts_and_maps_usage() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: GeminiResponseContent {
                    parts: vec![
                        GeminiResponsePart {
                            text: "Hello ".to_owned(),
                        },
                        GeminiResponsePart {
                            text: "there".to_owned(),
                        },
                    ],
                },
                finish_reason: Some("MAX_TOKENS".to_owned()),
            }],
            usage_metadata: GeminiUsage {
                prompt: 7,
                candidates: 4,
                total: 11,
            },
        };
        let out = translate_response(resp, "gemini-2.0");
        assert_eq!(
            out.choices[0].message.content.as_deref(),
            Some("Hello there")
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
        assert_eq!(out.usage.unwrap().total_tokens, 11);
        assert_eq!(out.model, "gemini-2.0");
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason(Some("STOP")).as_deref(), Some("stop"));
        assert_eq!(
            map_finish_reason(Some("SAFETY")).as_deref(),
            Some("content_filter")
        );
        assert_eq!(map_finish_reason(None), None);
    }
}
