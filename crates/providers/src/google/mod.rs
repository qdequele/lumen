//! Google Gemini provider - chat completions and embeddings.
//!
//! Embeddings are served through `batchEmbedContents` (see [`embed`], issue
//! #62). Gemini's `generateContent` API differs from OpenAI in several ways
//! this module bridges:
//!
//! * auth is an `x-goog-api-key` header (the key is never put in the URL);
//! * the model is part of the URL path (`/models/{model}:generateContent`,
//!   or `:streamGenerateContent?alt=sse` when streaming);
//! * messages are `contents` with roles `user`/`model` (assistant → `model`);
//!   system prompts go in a top-level `systemInstruction`;
//! * generation params live under `generationConfig`;
//! * responses are `candidates` with a `finishReason` and `usageMetadata`;
//! * streaming events are partial responses, translated fragment by fragment
//!   in [`stream`] (bounded state - the text is never accumulated).
//!
//! # Provider-native image sources (issue #12)
//!
//! An `image_url.url` recognised by `ImageUrl::gemini_file_uri` (a Gemini
//! Files API URI, or a `gs://` GCS URI) is translated to a
//! `fileData.fileUri` part and forwarded verbatim - never fetched.
//!
//! **`gs://` caveat**: the Gemini **Developer API**
//! (`generativelanguage.googleapis.com`, this provider's default base URL)
//! documents `fileData.fileUri` for its own Files API URIs; `gs://` Cloud
//! Storage URIs are a **Vertex AI** capability. A `gs://` reference is still
//! parsed and forwarded (the form is Gemini-native, so mismatch routing
//! stays an honest LM-2008, and `base_url` may point at a Vertex-compatible
//! gateway), but against the default endpoint the upstream will reject it -
//! that upstream error, naming this provider, is the honest outcome. See
//! `docs/providers.md`.

mod embed;
mod stream;
pub mod vertex;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{
    ChatChoice, ChatChunk, ChatMessage, ChatProvider, ChatRequest, ChatResponse, MessageContent,
    ProviderError, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt;
use tokio_util::sync::CancellationToken;

use self::stream::GoogleTranslator;
use crate::chat::{items_to_chunks, items_to_sse_bytes, translate_sse_stream, StreamItem};
use crate::http::{open_stream_with_headers, post_json_with_headers};

/// Default Gemini API base (the path adds `/v1beta/models/...`).
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// OpenAI chat fields `generateContent` has no equivalent for (issue #72):
/// no OpenAI-shaped logprobs (nor `top_logprobs`), no logit biasing, no
/// parallel-tool-call control. Rejected (strict) or dropped with a trace
/// (lenient) before any upstream call, by both the Gemini Developer API
/// provider and Vertex AI. `response_format`, `seed`, `frequency_penalty` and
/// `presence_penalty` are NOT here: they map natively onto `generationConfig`
/// in [`translate_request`].
pub(crate) const UNSUPPORTED_CHAT_FIELDS: &[&str] = &[
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "parallel_tool_calls",
];

/// A Google Gemini chat provider.
pub struct GoogleProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// API key sent as `x-goog-api-key`. Redacted from `Debug`; never logged,
    /// and never placed in the URL.
    api_key: Option<String>,
    /// When `true`, reject a request that sets an OpenAI field Gemini cannot
    /// honor ([`UNSUPPORTED_CHAT_FIELDS`]) with a 400 (`LM-1001`) instead of
    /// silently dropping it (issue #72).
    strict: bool,
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
            strict: false,
        }
    }

    /// Set strict mode: reject (400, `LM-1001`) rather than drop request
    /// fields Gemini cannot honor (issue #72). Defaults to `false` (lenient:
    /// drop with a `debug!` trace).
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
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
    /// OpenAI `tools` become one `tools[]` entry holding all
    /// `functionDeclarations`; omitted when the request carries no tools.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTool>,
    /// OpenAI `tool_choice` becomes `toolConfig.functionCallingConfig`;
    /// omitted (upstream default `AUTO` applies) when absent or unrecognised.
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<serde_json::Value>,
}

/// A Gemini `tools[]` entry. Gemini nests every function under one entry's
/// `functionDeclarations` array (unlike OpenAI's flat `tools` list).
#[derive(Serialize)]
struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// One Gemini function declaration: `{name, description?, parameters?}`, where
/// `parameters` is an OpenAPI-subset JSON schema (OpenAI's `parameters` maps
/// across directly).
#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(rename = "inline_data", skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
    #[serde(rename = "file_data", skip_serializing_if = "Option::is_none")]
    file_data: Option<GeminiFileData>,
    /// An assistant tool call: `{ "name": ..., "args": {...} }`. Mutually
    /// exclusive with the other fields.
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<serde_json::Value>,
    /// A tool result fed back to the model: `{ "name": ..., "response": {...} }`.
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    function_response: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

/// A provider-native file reference (issue #12): a Gemini Files API URI or a
/// GCS URI (`gs://...`). `mime_type` is included only when it could be
/// confidently inferred from the URI's file extension; otherwise it is
/// omitted rather than guessed. For a Files API URI that is always safe:
/// Gemini recorded the mime type at upload time and falls back to it. A
/// `gs://` object with an unrecognised extension has no such record on the
/// Developer API - but `gs://` is a Vertex AI capability there anyway (see
/// the module doc), so the upstream's own error is the honest outcome.
#[derive(Serialize)]
struct GeminiFileData {
    #[serde(rename = "mime_type", skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    #[serde(rename = "file_uri")]
    file_uri: String,
}

impl GeminiPart {
    /// A plain text part.
    fn text(s: String) -> Self {
        Self {
            text: Some(s),
            inline_data: None,
            file_data: None,
            function_call: None,
            function_response: None,
        }
    }

    /// An inline base64 image part (Gemini's only supported image input).
    fn image(mime_type: String, data: String) -> Self {
        Self {
            text: None,
            inline_data: Some(GeminiInlineData { mime_type, data }),
            file_data: None,
            function_call: None,
            function_response: None,
        }
    }

    /// An assistant tool-call part (`functionCall`).
    fn function_call(name: &str, args: &serde_json::Value) -> Self {
        Self {
            text: None,
            inline_data: None,
            file_data: None,
            function_call: Some(json!({ "name": name, "args": args })),
            function_response: None,
        }
    }

    /// A tool-result part (`functionResponse`). `response` must be a JSON object.
    fn function_response(name: &str, response: &serde_json::Value) -> Self {
        Self {
            text: None,
            inline_data: None,
            file_data: None,
            function_call: None,
            function_response: Some(json!({ "name": name, "response": response })),
        }
    }

    /// A provider-native file/GCS reference (issue #12).
    fn file(file_uri: String, mime_type: Option<String>) -> Self {
        Self {
            text: None,
            inline_data: None,
            file_data: Some(GeminiFileData {
                mime_type,
                file_uri,
            }),
            function_call: None,
            function_response: None,
        }
    }
}

/// Best-effort image mime type from a file URI's extension (issue #12). Only
/// a handful of well-known image extensions are recognised; anything else
/// (including a Gemini Files API URI, which never carries an extension)
/// yields `None` so the `mime_type` field is simply omitted rather than
/// guessed.
fn infer_image_mime_type(uri: &str) -> Option<&'static str> {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let (_, ext) = path.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        _ => None,
    }
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
    /// OpenAI `response_format` JSON mode -> `"application/json"` (issue #72).
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    /// OpenAI `response_format.json_schema.schema` -> Gemini's OpenAPI-subset
    /// schema (unsupported JSON Schema keys stripped - see
    /// [`sanitize_gemini_schema`]).
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<serde_json::Value>,
    /// OpenAI `seed` passes through natively (issue #72).
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
    /// OpenAI `frequency_penalty` -> Gemini's native field (issue #91).
    #[serde(rename = "frequencyPenalty", skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    /// OpenAI `presence_penalty` -> Gemini's native field (issue #91).
    #[serde(rename = "presencePenalty", skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
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
    #[serde(rename = "functionCall", default)]
    function_call: Option<GeminiFunctionCall>,
}

/// A model-emitted tool call: Gemini returns the arguments as a JSON object
/// (OpenAI carries them as a JSON string, so [`translate_response`] re-encodes).
#[derive(Deserialize)]
struct GeminiFunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: serde_json::Value,
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
///
/// # Errors
/// Returns [`ProviderError::ImageUrlNotSupported`] if a message carries a
/// remote (`http`/`https`) image URL - Gemini accepts only inline base64
/// image bytes, and the gateway never fetches a URL on the caller's behalf.
/// `provider_name` names the attempted link (which may be a fallback, not the
/// route the LM-2004 pre-flight checked - GH #13) so the error is attributed
/// correctly.
fn translate_request(
    req: &ChatRequest,
    provider_name: &str,
) -> Result<GeminiRequest, ProviderError> {
    // Tool traffic maps to parts: assistant `tool_calls` -> `functionCall`
    // (role `model`), role `tool` -> `functionResponse` (role `user`,
    // consecutive results merge into one content, matching Gemini's
    // user/model alternation). A `tool_call_id` -> function-name map lets a
    // `functionResponse` recover the name Gemini requires (OpenAI carries it
    // only on the originating `tool_call`).
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut system_parts: Vec<GeminiPart> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();
    for m in &req.messages {
        let text = m
            .content
            .as_ref()
            .map(|c| c.text().into_owned())
            .unwrap_or_default();
        match m.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system_parts.push(GeminiPart::text(text));
                }
            }
            "tool" => push_tool_result(&mut contents, &tool_names, m, &text),
            "assistant"
                if m.extra
                    .get("tool_calls")
                    .is_some_and(serde_json::Value::is_array) =>
            {
                push_assistant_tool_calls(&mut contents, &mut tool_names, m, &text);
            }
            // OpenAI's `assistant` is Gemini's `model`; everything else → user.
            role => contents.push(GeminiContent {
                role: if role == "assistant" {
                    "model".to_owned()
                } else {
                    "user".to_owned()
                },
                parts: gemini_parts(m.content.as_ref(), &text, provider_name)?,
            }),
        }
    }

    let stop_sequences = req
        .stop
        .as_ref()
        .map(collect_stop_sequences)
        .unwrap_or_default();
    let (response_mime_type, response_schema) = req
        .extra
        .get("response_format")
        .map(translate_response_format)
        .unwrap_or_default();
    let generation_config = GeminiGenerationConfig {
        max_output_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences,
        response_mime_type,
        response_schema,
        // A non-integer seed is invalid OpenAI input anyway; dropped, not guessed.
        seed: req.extra.get("seed").and_then(serde_json::Value::as_i64),
        // A non-numeric penalty is invalid OpenAI input; dropped, not guessed.
        frequency_penalty: req
            .extra
            .get("frequency_penalty")
            .and_then(serde_json::Value::as_f64),
        presence_penalty: req
            .extra
            .get("presence_penalty")
            .and_then(serde_json::Value::as_f64),
    };

    Ok(GeminiRequest {
        contents,
        system_instruction: if system_parts.is_empty() {
            None
        } else {
            Some(GeminiSystem {
                parts: system_parts,
            })
        },
        generation_config: Some(generation_config),
        tools: translate_tools(req),
        tool_config: req.extra.get("tool_choice").and_then(translate_tool_choice),
    })
}

/// Append a role-`tool` message as a Gemini `functionResponse` part, merging
/// into the previous user content when it already holds tool results (Gemini
/// expects strict user/model alternation).
fn push_tool_result(
    contents: &mut Vec<GeminiContent>,
    tool_names: &std::collections::HashMap<String, String>,
    m: &ChatMessage,
    text: &str,
) {
    let call_id = m
        .extra
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // Prefer the name recorded from the assistant call, then an explicit
    // `name`, then the id itself as a last resort.
    let name = tool_names
        .get(call_id)
        .cloned()
        .or_else(|| m.name.clone())
        .unwrap_or_else(|| call_id.to_owned());
    let part = GeminiPart::function_response(&name, &tool_response_value(text));
    match contents.last_mut() {
        Some(prev)
            if prev.role == "user" && prev.parts.iter().any(|p| p.function_response.is_some()) =>
        {
            prev.parts.push(part);
        }
        _ => contents.push(GeminiContent {
            role: "user".to_owned(),
            parts: vec![part],
        }),
    }
}

/// Append an assistant message carrying `tool_calls` as a Gemini `model`
/// content with `functionCall` parts (preceded by a text part when present),
/// recording each call id -> name so a later `functionResponse` can name it.
fn push_assistant_tool_calls(
    contents: &mut Vec<GeminiContent>,
    tool_names: &mut std::collections::HashMap<String, String>,
    m: &ChatMessage,
    text: &str,
) {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(GeminiPart::text(text.to_owned()));
    }
    if let Some(calls) = m.extra.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                tool_names.insert(id.to_owned(), name.to_owned());
            }
            parts.push(GeminiPart::function_call(
                name,
                &parse_tool_arguments(call.pointer("/function/arguments")),
            ));
        }
    }
    contents.push(GeminiContent {
        role: "model".to_owned(),
        parts,
    });
}

/// OpenAI tool-call `arguments` is a JSON *string*; Gemini `args` is the object
/// itself. Unparseable or absent arguments degrade to an empty object.
fn parse_tool_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    arguments
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}))
}

/// A tool result is an opaque string to OpenAI, but Gemini's `functionResponse`
/// `response` must be a JSON *object*. A result that already is a JSON object
/// passes through; anything else is wrapped as `{ "result": <text> }`.
fn tool_response_value(text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => json!({ "result": text }),
    }
}

/// OpenAI `tools` (`{type: "function", function: {name, description,
/// parameters}}`) → one Gemini `tools[]` entry whose `functionDeclarations`
/// hold every function. Non-function entries are skipped.
fn translate_tools(req: &ChatRequest) -> Vec<GeminiTool> {
    let Some(tools) = req.extra.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(GeminiFunctionDeclaration {
                name: function.get("name")?.as_str()?.to_owned(),
                description: function
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                parameters: function.get("parameters").cloned(),
            })
        })
        .collect();
    if declarations.is_empty() {
        Vec::new()
    } else {
        vec![GeminiTool {
            function_declarations: declarations,
        }]
    }
}

/// OpenAI `tool_choice` → Gemini `toolConfig.functionCallingConfig`. Unknown
/// shapes are dropped (the upstream default, `AUTO`, applies) rather than
/// guessed.
fn translate_tool_choice(choice: &serde_json::Value) -> Option<serde_json::Value> {
    match choice {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "functionCallingConfig": { "mode": "AUTO" } })),
            "required" => Some(json!({ "functionCallingConfig": { "mode": "ANY" } })),
            "none" => Some(json!({ "functionCallingConfig": { "mode": "NONE" } })),
            _ => None,
        },
        serde_json::Value::Object(_) => choice
            .pointer("/function/name")
            .and_then(|v| v.as_str())
            .map(|name| {
                json!({
                    "functionCallingConfig": {
                        "mode": "ANY",
                        "allowedFunctionNames": [name],
                    }
                })
            }),
        _ => None,
    }
}

/// Build Gemini `parts` from a message: data-URI images become `inline_data`;
/// a remote image URL is a client-input error (LM-2004 - Gemini takes only
/// inline bytes, and the gateway never fetches the URL). Text-only content is
/// one text part.
fn gemini_parts(
    content: Option<&MessageContent>,
    text: &str,
    provider_name: &str,
) -> Result<Vec<GeminiPart>, ProviderError> {
    match content {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut out = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    if let Some(uri) = img.gemini_file_uri() {
                        // Provider-native GCS / Files API reference (issue #12).
                        let mime_type = infer_image_mime_type(uri).map(str::to_owned);
                        out.push(GeminiPart::file(uri.to_owned(), mime_type));
                        continue;
                    }
                    let data =
                        img.as_data_uri()
                            .ok_or_else(|| ProviderError::ImageUrlNotSupported {
                                provider: provider_name.to_owned(),
                            })?;
                    out.push(GeminiPart::image(data.media_type, data.base64_data));
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        out.push(GeminiPart::text(t.clone()));
                    }
                }
            }
            Ok(out)
        }
        _ => Ok(vec![GeminiPart::text(text.to_owned())]),
    }
}

/// OpenAI `response_format` -> Gemini `generationConfig` JSON-mode fields
/// (issue #72): `{"type": "json_object"}` becomes
/// `responseMimeType: "application/json"`; `{"type": "json_schema"}`
/// additionally carries `json_schema.schema` as `responseSchema` (sanitized to
/// Gemini's OpenAPI subset). `{"type": "text"}` is the default and unknown
/// shapes are dropped with a `debug` trace, not guessed (matching the
/// `tool_choice` precedent).
fn translate_response_format(
    format: &serde_json::Value,
) -> (Option<String>, Option<serde_json::Value>) {
    const JSON_MIME: &str = "application/json";
    match format.get("type").and_then(|v| v.as_str()) {
        Some("json_object") => (Some(JSON_MIME.to_owned()), None),
        Some("json_schema") => {
            let schema = format.pointer("/json_schema/schema").cloned().map(|mut s| {
                sanitize_gemini_schema(&mut s);
                s
            });
            if schema.is_none() {
                tracing::debug!(
                    "gemini chat: response_format type json_schema without a \
                     json_schema.schema object; JSON mode applied without a schema"
                );
            }
            (Some(JSON_MIME.to_owned()), schema)
        }
        Some("text") => (None, None),
        None => {
            tracing::debug!("gemini chat: response_format without a type string dropped");
            (None, None)
        }
        Some(other) => {
            tracing::debug!(
                response_format_type = other,
                "gemini chat: unrecognised response_format type dropped"
            );
            (None, None)
        }
    }
}

/// Strip JSON Schema keywords Gemini's OpenAPI-subset `responseSchema` rejects
/// (`additionalProperties`, `$schema`), recursing only through positions that
/// hold sub-SCHEMAS (`properties` values, `items`, `anyOf`/`allOf`/`oneOf`
/// entries) so a property literally named `additionalProperties` is untouched.
fn sanitize_gemini_schema(schema: &mut serde_json::Value) {
    let Some(map) = schema.as_object_mut() else {
        return;
    };
    for keyword in ["additionalProperties", "$schema"] {
        if map.remove(keyword).is_some() {
            tracing::debug!(
                keyword,
                "gemini chat: stripped a JSON Schema keyword Gemini's \
                 responseSchema does not accept (it is dropped, not enforced)"
            );
        }
    }
    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
        for sub in props.values_mut() {
            sanitize_gemini_schema(sub);
        }
    }
    if let Some(items) = map.get_mut("items") {
        sanitize_gemini_schema(items);
    }
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(subs) = map.get_mut(key).and_then(|v| v.as_array_mut()) {
            for sub in subs {
                sanitize_gemini_schema(sub);
            }
        }
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
    let mut text = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    let mut finish_reason = None;
    if let Some(candidate) = resp.candidates.into_iter().next() {
        finish_reason = map_finish_reason(candidate.finish_reason.as_deref());
        for part in candidate.content.parts {
            if let Some(call) = part.function_call {
                // Gemini omits a call id; synthesize a stable, per-response one.
                let index = tool_calls.len();
                let args = if call.args.is_null() {
                    json!({})
                } else {
                    call.args
                };
                tool_calls.push(json!({
                    "id": format!("call_{index}"),
                    "type": "function",
                    "function": {
                        "name": call.name,
                        // OpenAI carries arguments as a JSON string.
                        "arguments": args.to_string(),
                    },
                }));
            } else {
                text.push_str(&part.text);
            }
        }
    }

    let mut extra = serde_json::Map::new();
    if !tool_calls.is_empty() {
        // Gemini reports `STOP` even for tool calls; OpenAI expects `tool_calls`.
        finish_reason = Some("tool_calls".to_owned());
        extra.insert(
            "tool_calls".to_owned(),
            serde_json::Value::Array(tool_calls),
        );
    }
    // OpenAI uses `content: null` for pure tool-call messages.
    let content = if text.is_empty() && !extra.is_empty() {
        None
    } else {
        Some(MessageContent::Text(text))
    };

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
                content,
                name: None,
                extra,
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
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        // The model is part of the path; the key is a header, never the URL.
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, req.model
        );
        let body = translate_request(&req, &self.provider_name)?;
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

    /// Gemini accepts only inline base64 image bytes, never a fetchable URL -
    /// the gateway must not fetch on the caller's behalf (LM-2004 pre-flight
    /// in the handler; [`translate_request`] is the defensive fallback path).
    fn accepts_remote_image_url(&self) -> bool {
        false
    }

    /// Gemini is the only provider that can resolve its own GCS / Files API
    /// `fileUri` references (issue #12); a mismatch is caught pre-flight
    /// with `LM-2008`.
    fn accepts_gemini_file_uri(&self) -> bool {
        true
    }
}

/// Fuzz-only shims over the private translation functions above.
///
/// `cargo fuzz` builds the whole dependency graph (including this crate)
/// with `--cfg fuzzing` set, so these functions compile only under
/// `cargo +nightly fuzz run ...` and add nothing to normal builds (`cargo
/// build`/`clippy`/`test` never set this cfg). This is the least invasive
/// way to reach `translate_request`/`translate_response`: no visibility
/// changes to the wire types (`GeminiRequest`/`GeminiResponse` stay
/// private), no new public API surface outside fuzzing builds.
#[cfg(fuzzing)]
pub mod fuzzing {
    use super::{translate_request, translate_response, GeminiResponse};
    use lumen_core::ChatRequest;

    /// Translate an arbitrary `ChatRequest` and serialize the result on
    /// success; must never panic regardless of message/image shape.
    pub fn fuzz_translate_request(req: &ChatRequest) {
        if let Ok(translated) = translate_request(req, "fuzz") {
            let _ = serde_json::to_vec(&translated);
        }
    }

    /// Deserialize arbitrary bytes as a Gemini response and translate
    /// whatever parses; must never panic on malformed or adversarial input.
    pub fn fuzz_translate_response(data: &[u8], requested_model: &str) {
        if let Ok(resp) = serde_json::from_slice::<GeminiResponse>(data) {
            let _ = translate_response(resp, requested_model);
        }
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
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        // `alt=sse` selects SSE framing; the key stays in a header, never the URL.
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
        let body = translate_request(&req, &self.provider_name)?;
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
            content: Some(MessageContent::Text(content.to_owned())),
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
        let out = translate_request(
            &request(vec![
                msg("system", "be brief"),
                msg("user", "hi"),
                msg("assistant", "hello"),
                msg("user", "more"),
            ]),
            "google",
        )
        .unwrap();
        assert_eq!(
            out.system_instruction.as_ref().unwrap().parts[0]
                .text
                .as_deref(),
            Some("be brief")
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
                            function_call: None,
                        },
                        GeminiResponsePart {
                            text: "there".to_owned(),
                            function_call: None,
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
            out.choices[0]
                .message
                .content
                .as_ref()
                .map(|c| c.text().into_owned()),
            Some("Hello there".to_owned())
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

    #[test]
    fn data_uri_image_becomes_inline_data() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "gemini".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![
                    ContentPart {
                        kind: "text".to_owned(),
                        text: Some("what?".to_owned()),
                        image_url: None,
                        extra: serde_json::Map::new(),
                    },
                    ContentPart {
                        kind: "image_url".to_owned(),
                        text: None,
                        image_url: Some(ImageUrl {
                            url: "data:image/jpeg;base64, /9j/".to_owned().replace(' ', ""),
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
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts[0]["text"], "what?");
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/jpeg");
        assert_eq!(parts[1]["inline_data"]["data"], "/9j/");
    }

    /// An OpenAI request WITH tools translates to the exact expected Gemini
    /// JSON: `tools[].functionDeclarations`, `toolConfig`, an assistant
    /// `functionCall` part, and a `functionResponse` part (request side).
    #[test]
    fn request_with_tools_matches_expected_gemini_json_exactly() {
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
            model: "gemini-2.0".to_owned(),
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
                    content: Some(MessageContent::Text("18C, sunny".to_owned())),
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

        let out = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert_eq!(
            out,
            json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "weather in Paris?" }] },
                    { "role": "model", "parts": [{
                        "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }
                    }] },
                    { "role": "user", "parts": [{
                        "functionResponse": {
                            "name": "get_weather",
                            "response": { "result": "18C, sunny" }
                        }
                    }] }
                ],
                "systemInstruction": { "parts": [{ "text": "be brief" }] },
                "generationConfig": { "maxOutputTokens": 1024 },
                "tools": [{
                    "functionDeclarations": [{
                        "name": "get_weather",
                        "description": "Weather lookup",
                        "parameters": {
                            "type": "object",
                            "properties": { "city": { "type": "string" } },
                            "required": ["city"]
                        }
                    }]
                }],
                "toolConfig": { "functionCallingConfig": { "mode": "AUTO" } }
            })
        );
    }

    #[test]
    fn tool_choice_variants_map_to_function_calling_config() {
        assert_eq!(
            translate_tool_choice(&json!("auto")),
            Some(json!({ "functionCallingConfig": { "mode": "AUTO" } }))
        );
        assert_eq!(
            translate_tool_choice(&json!("required")),
            Some(json!({ "functionCallingConfig": { "mode": "ANY" } }))
        );
        assert_eq!(
            translate_tool_choice(&json!("none")),
            Some(json!({ "functionCallingConfig": { "mode": "NONE" } }))
        );
        assert_eq!(
            translate_tool_choice(&json!({
                "type": "function", "function": { "name": "f" }
            })),
            Some(json!({
                "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["f"] }
            }))
        );
        // Unknown shapes are dropped, not guessed.
        assert_eq!(translate_tool_choice(&json!(42)), None);
    }

    #[test]
    fn response_function_call_becomes_openai_tool_calls() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: GeminiResponseContent {
                    parts: vec![GeminiResponsePart {
                        text: String::new(),
                        function_call: Some(GeminiFunctionCall {
                            name: "get_weather".to_owned(),
                            args: json!({ "city": "Paris" }),
                        }),
                    }],
                },
                finish_reason: Some("STOP".to_owned()),
            }],
            usage_metadata: GeminiUsage {
                prompt: 4,
                candidates: 2,
                total: 6,
            },
        };
        let out = translate_response(resp, "gemini-2.0");
        let message = &out.choices[0].message;
        // Pure tool-call message: content is null, tool_calls carry the call.
        assert_eq!(message.content, None);
        let calls = message.extra["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!({ "city": "Paris" }).to_string()
        );
        // A Gemini `STOP` alongside a function call maps to OpenAI `tool_calls`.
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn remote_url_image_is_image_url_not_supported_naming_the_attempted_link() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "gemini".to_owned(),
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
        // The provider name passed in is the link actually attempted (which
        // may be a fallback, not the route the LM-2004 pre-flight checked -
        // GH #13), so it must come through in the error unchanged.
        match translate_request(&req, "gemini-fallback") {
            Ok(_) => panic!("expected the remote URL to be rejected"),
            Err(lumen_core::ProviderError::ImageUrlNotSupported { provider }) => {
                assert_eq!(provider, "gemini-fallback");
            }
            Err(other) => panic!("expected ImageUrlNotSupported, got {other:?}"),
        }
    }

    /// Issue #12: a `gs://` GCS URI becomes a `file_data` part carrying the
    /// URI verbatim, with a mime type inferred from the extension.
    #[test]
    fn gcs_uri_becomes_a_file_data_part_with_inferred_mime_type() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "gemini".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![
                    ContentPart {
                        kind: "text".to_owned(),
                        text: Some("what?".to_owned()),
                        image_url: None,
                        extra: serde_json::Map::new(),
                    },
                    ContentPart {
                        kind: "image_url".to_owned(),
                        text: None,
                        image_url: Some(ImageUrl {
                            url: "gs://my-bucket/cat.png".to_owned(),
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
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts[0]["text"], "what?");
        assert_eq!(parts[1]["file_data"]["file_uri"], "gs://my-bucket/cat.png");
        assert_eq!(parts[1]["file_data"]["mime_type"], "image/png");
        assert!(parts[1].get("inline_data").is_none());
    }

    /// A Gemini Files API URI never carries a file extension; the gateway
    /// omits `mime_type` rather than guessing (Gemini already knows it from
    /// the upload).
    #[test]
    fn gemini_files_api_uri_omits_mime_type_when_extension_is_unknown() {
        use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
        let req = ChatRequest {
            model: "gemini".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Parts(vec![ContentPart {
                    kind: "image_url".to_owned(),
                    text: None,
                    image_url: Some(ImageUrl {
                        url: "https://generativelanguage.googleapis.com/v1beta/files/abc-123"
                            .to_owned(),
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
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(
            part["file_data"]["file_uri"],
            "https://generativelanguage.googleapis.com/v1beta/files/abc-123"
        );
        assert!(part["file_data"].get("mime_type").is_none());
    }

    /// Issue #72: `response_format: {"type": "json_object"}` maps to Gemini's
    /// native JSON mode instead of being silently dropped.
    #[test]
    fn response_format_json_object_maps_to_response_mime_type() {
        let mut req = request(vec![msg("user", "hi")]);
        req.extra.insert(
            "response_format".to_owned(),
            json!({ "type": "json_object" }),
        );
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert!(body["generationConfig"].get("responseSchema").is_none());
    }

    /// Issue #72: `json_schema` carries the schema through as `responseSchema`,
    /// with JSON Schema keys Gemini rejects stripped (`additionalProperties`,
    /// `$schema`) - but a *property* named `additionalProperties` survives.
    #[test]
    fn response_format_json_schema_maps_to_sanitized_response_schema() {
        let mut req = request(vec![msg("user", "hi")]);
        req.extra.insert(
            "response_format".to_owned(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "city",
                    "strict": true,
                    "schema": {
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "name": { "type": "string" },
                            "additionalProperties": { "type": "boolean" },
                            "tags": {
                                "type": "array",
                                "items": { "type": "object", "additionalProperties": false }
                            }
                        },
                        "required": ["name"]
                    }
                }
            }),
        );
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        let cfg = &body["generationConfig"];
        assert_eq!(cfg["responseMimeType"], "application/json");
        let schema = &cfg["responseSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema.get("additionalProperties").is_none());
        assert!(schema.get("$schema").is_none());
        // The property literally named `additionalProperties` is data, not a
        // schema keyword: it must survive.
        assert_eq!(
            schema["properties"]["additionalProperties"]["type"],
            "boolean"
        );
        assert!(schema["properties"]["tags"]["items"]
            .get("additionalProperties")
            .is_none());
    }

    /// Issue #72: `seed` maps to `generationConfig.seed`; `type: "text"` and
    /// unknown response_format shapes emit no JSON-mode fields.
    #[test]
    fn seed_maps_natively_and_text_or_unknown_formats_are_dropped() {
        let mut req = request(vec![msg("user", "hi")]);
        req.extra.insert("seed".to_owned(), json!(42));
        req.extra
            .insert("response_format".to_owned(), json!({ "type": "text" }));
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert_eq!(body["generationConfig"]["seed"], 42);
        assert!(body["generationConfig"].get("responseMimeType").is_none());

        req.extra
            .insert("response_format".to_owned(), json!({ "type": "surprise" }));
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert!(body["generationConfig"].get("responseMimeType").is_none());
    }

    /// Issue #91: `frequency_penalty` / `presence_penalty` map to Gemini's
    /// native `generationConfig.frequencyPenalty` / `presencePenalty` instead
    /// of being silently dropped; a non-numeric value is dropped, not guessed.
    #[test]
    fn frequency_and_presence_penalty_map_to_generation_config() {
        let mut req = request(vec![msg("user", "hi")]);
        req.extra.insert("frequency_penalty".to_owned(), json!(0.5));
        req.extra
            .insert("presence_penalty".to_owned(), json!(-0.25));
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert_eq!(body["generationConfig"]["frequencyPenalty"], 0.5);
        assert_eq!(body["generationConfig"]["presencePenalty"], -0.25);

        // A non-numeric penalty is invalid OpenAI input: dropped, not guessed.
        let mut req = request(vec![msg("user", "hi")]);
        req.extra
            .insert("frequency_penalty".to_owned(), json!("nope"));
        let body = serde_json::to_value(translate_request(&req, "google").unwrap()).unwrap();
        assert!(body["generationConfig"].get("frequencyPenalty").is_none());
        assert!(body["generationConfig"].get("presencePenalty").is_none());
    }

    /// Issue #72: strict mode rejects fields Gemini cannot honor with an
    /// honest 400 (`UnsupportedField` -> LM-1001) BEFORE any upstream call
    /// (the base URL is unroutable on purpose); lenient mode proceeds.
    #[tokio::test]
    async fn strict_mode_rejects_logprobs_and_parallel_tool_calls() {
        use lumen_core::ChatProvider as _;
        use tokio_util::sync::CancellationToken;
        let provider = GoogleProvider::new(
            reqwest::Client::new(),
            "google-test",
            Some("http://127.0.0.1:1".to_owned()),
            Some("goog-test".to_owned()),
        )
        .with_strict(true);

        for (field, value) in [
            ("logprobs", json!(true)),
            ("top_logprobs", json!(5)),
            ("logit_bias", json!({ "50256": -100 })),
            ("parallel_tool_calls", json!(false)),
        ] {
            let mut req = request(vec![msg("user", "hi")]);
            req.extra.insert(field.to_owned(), value);
            let err = provider
                .chat(req.clone(), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                matches!(
                    &err,
                    ProviderError::UnsupportedField { provider, field: f }
                        if provider == "google-test" && f == field
                ),
                "expected UnsupportedField for {field}, got {err:?}"
            );
            let err = provider
                .chat_stream(req, CancellationToken::new())
                .await
                .err()
                .expect("stream must be rejected too");
            assert!(matches!(err, ProviderError::UnsupportedField { .. }));
        }
    }

    #[test]
    fn google_provider_accepts_its_own_file_uri() {
        let provider = GoogleProvider::new(
            reqwest::Client::new(),
            "google".to_owned(),
            None,
            Some("goog-test".to_owned()),
        );
        assert!(provider.accepts_gemini_file_uri());
        assert!(!provider.accepts_anthropic_file_id());
        assert!(!provider.accepts_remote_image_url());
    }
}
