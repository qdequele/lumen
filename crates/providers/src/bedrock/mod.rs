//! AWS Bedrock provider - chat completions over the Converse API.
//!
//! Bedrock exposes many model families (Anthropic, Meta Llama, Amazon Titan/Nova,
//! Mistral, Cohere) behind ONE uniform schema: the Converse API
//! (`POST /model/{modelId}/converse` and `/converse-stream`). This provider
//! targets Converse exclusively, which collapses the "per-model request/response
//! schema" problem of the legacy `InvokeModel` API into a single translation.
//! `InvokeModel` is intentionally not implemented (Converse covers the same
//! models with one schema).
//!
//! What this module bridges from OpenAI's `chat/completions`:
//!
//! * the model id lives in the URL PATH, not the body;
//! * `system` prompts are a top-level `system` block array, not a message;
//! * inference params (`maxTokens`, `temperature`, `topP`, `stopSequences`)
//!   live under `inferenceConfig`;
//! * tool traffic maps to `toolUse` / `toolResult` content blocks;
//! * auth is AWS SigV4 request signing (see [`sigv4`]), not a bearer token;
//! * streaming is AWS event-stream binary framing (see [`eventstream`]),
//!   translated frame by frame in [`stream`] (bounded state).
//!
//! Region drives the endpoint host `bedrock-runtime.{region}.amazonaws.com`
//! and the SigV4 signing scope. It is resolved from the configured `base_url`
//! host (standard and VPC-endpoint shapes), else from `AWS_REGION` /
//! `AWS_DEFAULT_REGION`; when neither yields a region the registry fails the
//! build with a clear error instead of silently signing for a wrong region.
//!
//! Credentials (access key id + secret, plus an optional session token) live in
//! a [`Credentials`] value whose `Debug` never reveals the secrets. When built
//! from the registry the provider re-reads the standard AWS environment
//! variables on EVERY request-signing call (cheap: three env lookups), so
//! credentials updated in the process environment - or a config hot reload
//! rebuilding the provider - take effect without a restart. A full AWS
//! credential-provider chain (IMDS, SSO, profiles, `credential_process`) is
//! intentionally out of scope for v1: only static keys and pre-issued STS
//! session tokens are supported, and an expired session token keeps failing
//! (403) until the environment provides a fresh one.

mod embed;
mod eventstream;
mod sigv4;
mod stream;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use lumen_core::{
    ChatChoice, ChatChunk, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ImageUrl,
    MessageContent, ProviderError, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use self::sigv4::{sign_request, uri_encode_segment, SigningParams};
use self::stream::{translate_eventstream, BedrockStreamTranslator};
use crate::chat::{items_to_chunks, items_to_sse_bytes};
use crate::http::{map_transport, with_cancel};
use crate::mapping::{classify_status, parse_retry_after};

/// Where a provider's signing credentials come from. Kept private: the public
/// constructors ([`BedrockProvider::new`] and
/// [`BedrockProvider::new_with_env_credentials`]) select the variant.
enum CredentialSource {
    /// Fixed credentials captured at construction (tests, embedders). Never
    /// refreshed.
    Static(Credentials),
    /// Re-read the standard AWS environment variables on every signing call,
    /// with an optional secret-access-key override from `api_key_env`.
    Env { secret_override: Option<String> },
    /// No credentials were provided; every request fails with a clear error.
    Missing,
}

// Manual Debug: `Env.secret_override` is a secret and must never leak through
// a stray `{:?}` (CLAUDE.md rule 5). `Static` defers to `Credentials`' own
// redacting Debug.
impl fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialSource::Static(creds) => f.debug_tuple("Static").field(creds).finish(),
            CredentialSource::Env { secret_override } => f
                .debug_struct("Env")
                .field(
                    "secret_override",
                    &secret_override.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
            CredentialSource::Missing => write!(f, "Missing"),
        }
    }
}

/// Resolved AWS credentials. The secret access key and session token are
/// secrets: this type's `Debug` reveals only the (public) access key id, so a
/// stray `{:?}` can never leak signing material (CLAUDE.md rule 5).
#[derive(Clone)]
pub struct Credentials {
    /// AWS access key id (public half).
    pub access_key_id: String,
    /// AWS secret access key (secret).
    secret_access_key: String,
    /// Optional STS session token (secret), for temporary credentials.
    session_token: Option<String>,
}

impl Credentials {
    /// Construct credentials from their parts.
    #[must_use]
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
    ) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token,
        }
    }

    /// Resolve credentials from the standard AWS environment variables
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and the optional
    /// `AWS_SESSION_TOKEN`), overriding the secret with `api_key_override` when
    /// the operator configured one via `api_key_env`. Returns `None` when the
    /// access key id or secret is absent (the provider then fails cleanly at
    /// request time rather than sending an unsigned request).
    #[must_use]
    pub fn from_env(api_key_override: Option<String>) -> Option<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret_access_key =
            api_key_override.or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
        Some(Self::new(access_key_id, secret_access_key, session_token))
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Derive the AWS region from a configured `base_url` host. Handles the
/// standard runtime endpoint (`bedrock-runtime.{region}.amazonaws.com`) and
/// VPC/PrivateLink endpoint shapes (`bedrock-runtime.{region}.vpce.amazonaws.com`,
/// `vpce-xxx.bedrock-runtime.{region}.vpce.amazonaws.com`): the label following a
/// `bedrock-runtime` label is taken when it looks like a region. Returns `None`
/// for any other host (e.g. a test mock), where the region must come from the
/// environment instead - see [`resolve_region`].
#[must_use]
pub fn region_from_base_url(base_url: Option<&str>) -> Option<String> {
    let url = base_url?;
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split('/')
        .next()?;
    let labels: Vec<&str> = host.split('.').collect();
    let runtime_index = labels.iter().position(|l| *l == "bedrock-runtime")?;
    let candidate = labels.get(runtime_index + 1)?;
    if looks_like_region(candidate) {
        Some((*candidate).to_owned())
    } else {
        None
    }
}

/// Whether a host label plausibly names an AWS region (`us-east-1`,
/// `ap-southeast-2`, `us-gov-west-1`, ...): lowercase alphanumerics and
/// hyphens, at least one hyphen, starting with a letter and ending with a
/// digit. Deliberately shape-based, not a hard-coded region list.
fn looks_like_region(label: &str) -> bool {
    label.contains('-')
        && label.starts_with(|c: char| c.is_ascii_lowercase())
        && label.ends_with(|c: char| c.is_ascii_digit())
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Resolve the signing region: from the `base_url` host if it carries one
/// (standard or VPC endpoint shapes), else from the `AWS_REGION` /
/// `AWS_DEFAULT_REGION` environment variables. `None` means the region cannot
/// be determined; the registry turns that into a build error rather than
/// silently signing with a default region (which would 403 against any other
/// region's endpoint).
#[must_use]
pub fn resolve_region(base_url: Option<&str>) -> Option<String> {
    region_from_base_url(base_url)
        .or_else(|| std::env::var("AWS_REGION").ok().filter(|r| !r.is_empty()))
        .or_else(|| {
            std::env::var("AWS_DEFAULT_REGION")
                .ok()
                .filter(|r| !r.is_empty())
        })
}

/// OpenAI chat fields the Converse API has no equivalent for (issues #72,
/// #91): no JSON mode / structured output, no sampling seed, no logprobs (nor
/// `top_logprobs`), no logit biasing, no parallel-tool-call control, no
/// frequency/presence penalties. Rejected (strict) or dropped with a trace
/// (lenient) before any upstream call.
const UNSUPPORTED_CHAT_FIELDS: &[&str] = &[
    "response_format",
    "seed",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "parallel_tool_calls",
    "frequency_penalty",
    "presence_penalty",
];

/// An AWS Bedrock chat provider (Converse API).
pub struct BedrockProvider {
    client: reqwest::Client,
    provider_name: String,
    /// AWS region for signing scope and the default endpoint host.
    region: String,
    /// Endpoint base URL (no trailing slash), e.g.
    /// `https://bedrock-runtime.us-east-1.amazonaws.com`.
    endpoint: String,
    /// Where signing credentials come from (static, env-per-request, or
    /// missing - the latter fails every request with a clear error).
    credentials: CredentialSource,
    /// When `true`, reject a request that sets an OpenAI field Converse cannot
    /// honor ([`UNSUPPORTED_CHAT_FIELDS`]) with a 400 (`LM-1001`) instead of
    /// silently dropping it (issue #72).
    strict: bool,
}

impl BedrockProvider {
    /// Construct a provider with FIXED credentials. `region` names the signing
    /// region; `base_url` overrides the endpoint (else the region's public
    /// runtime host is used); `credentials` are the SigV4 signing credentials
    /// (`None` fails every request cleanly). Static credentials are never
    /// refreshed - the registry path uses
    /// [`new_with_env_credentials`](Self::new_with_env_credentials) instead.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        region: impl Into<String>,
        base_url: Option<String>,
        credentials: Option<Credentials>,
    ) -> Self {
        let source = credentials.map_or(CredentialSource::Missing, CredentialSource::Static);
        Self::build(client, provider_name, region, base_url, source)
    }

    /// Construct a provider whose credentials are re-read from the standard AWS
    /// environment variables on every request-signing call, so rotated values
    /// take effect without a restart. `secret_override` (from `api_key_env`)
    /// replaces only the secret access key when set.
    #[must_use]
    pub fn new_with_env_credentials(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        region: impl Into<String>,
        base_url: Option<String>,
        secret_override: Option<String>,
    ) -> Self {
        Self::build(
            client,
            provider_name,
            region,
            base_url,
            CredentialSource::Env { secret_override },
        )
    }

    fn build(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        region: impl Into<String>,
        base_url: Option<String>,
        credentials: CredentialSource,
    ) -> Self {
        let region = region.into();
        let endpoint = base_url
            .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"))
            .trim_end_matches('/')
            .to_owned();
        Self {
            client,
            provider_name: provider_name.into(),
            region,
            endpoint,
            credentials,
            strict: false,
        }
    }

    /// Set strict mode: reject (400, `LM-1001`) rather than drop request
    /// fields the Converse API cannot honor (issue #72). Defaults to `false`
    /// (lenient: drop with a `debug!` trace).
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Resolve the credentials to sign THIS request with. The env source
    /// re-reads the environment on every call (three cheap lookups) so updated
    /// values are picked up per request.
    fn request_credentials(&self) -> Result<Credentials, ProviderError> {
        let missing = || {
            // No secret leaks: this only reports that config is incomplete.
            ProviderError::Translation(format!(
                "bedrock provider '{}' has no AWS credentials configured \
                 (set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY)",
                self.provider_name
            ))
        };
        match &self.credentials {
            CredentialSource::Static(creds) => Ok(creds.clone()),
            CredentialSource::Env { secret_override } => {
                Credentials::from_env(secret_override.clone()).ok_or_else(missing)
            }
            CredentialSource::Missing => Err(missing()),
        }
    }

    /// The bare `Host` header value (endpoint with the scheme stripped).
    fn host(&self) -> &str {
        self.endpoint
            .split_once("://")
            .map_or(self.endpoint.as_str(), |(_, rest)| rest)
    }

    /// The wire request path for a Converse action, with the model id
    /// percent-encoded ONCE (the signer double-encodes it again for the
    /// canonical request, per the non-S3 SigV4 rule - see [`sigv4`]).
    fn path(model: &str, action: &str) -> String {
        format!("/model/{}/{action}", uri_encode_segment(model))
    }

    /// Build a signed request builder for the wire `path` carrying
    /// `body_bytes`. The signature covers exactly `body_bytes`, which are also
    /// what gets sent.
    fn signed_request(
        &self,
        path: &str,
        body_bytes: Vec<u8>,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let creds = self.request_credentials()?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let signed = sign_request(
            &SigningParams {
                access_key_id: &creds.access_key_id,
                secret_access_key: &creds.secret_access_key,
                session_token: creds.session_token.as_deref(),
                region: &self.region,
            },
            self.host(),
            path,
            &body_bytes,
            now,
        );

        let url = format!("{}{path}", self.endpoint);
        let mut builder = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.content_sha256)
            .header("authorization", &signed.authorization)
            .body(body_bytes);
        if let Some(token) = &signed.security_token {
            builder = builder.header("x-amz-security-token", token);
        }
        Ok(builder)
    }

    /// Send a signed non-streaming Converse request, honouring `cancel`.
    async fn send(
        &self,
        path: &str,
        body_bytes: Vec<u8>,
        cancel: &CancellationToken,
    ) -> Result<Bytes, ProviderError> {
        let builder = self.signed_request(path, body_bytes)?;
        let provider = &self.provider_name;
        let call = async {
            let response = builder
                .send()
                .await
                .map_err(|e| map_transport(provider, &e))?;
            let status = response.status();
            if status.is_success() {
                response
                    .bytes()
                    .await
                    .map_err(|e| map_transport(provider, &e))
            } else {
                let retry_after = parse_retry_after(response.headers());
                Err(classify_status(provider, status.as_u16(), retry_after))
            }
        };
        with_cancel(cancel, call).await
    }

    /// Open a signed streaming Converse request, returning the raw byte stream
    /// of AWS event-stream frames. Honours `cancel` for the initial send; the
    /// returned stream aborts when dropped (ADR 004).
    async fn open(
        &self,
        path: &str,
        body_bytes: Vec<u8>,
        cancel: &CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let builder = self.signed_request(path, body_bytes)?;
        let provider = self.provider_name.clone();
        let call = async {
            let response = builder
                .send()
                .await
                .map_err(|e| map_transport(&provider, &e))?;
            let status = response.status();
            if status.is_success() {
                Ok(response)
            } else {
                let retry_after = parse_retry_after(response.headers());
                Err(classify_status(&provider, status.as_u16(), retry_after))
            }
        };
        let response = with_cancel(cancel, call).await?;
        let provider = self.provider_name.clone();
        Ok(response
            .bytes_stream()
            .map(move |item| item.map_err(|e| map_transport(&provider, &e)))
            .boxed())
    }

    /// Open the upstream stream and translate its event-stream frames.
    async fn open_translated_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<crate::chat::StreamItem, ProviderError>>, ProviderError>
    {
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        let body = serialize_body(&req)?;
        let path = Self::path(&req.model, "converse-stream");
        let bytes = self.open(&path, body, &cancel).await?;
        let translator = BedrockStreamTranslator::new(generated_id(), &req.model, now_secs());
        Ok(translate_eventstream(bytes, translator))
    }
}

impl fmt::Debug for BedrockProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockProvider")
            .field("provider_name", &self.provider_name)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

// ---- Wire types (Converse request/response) -------------------------------

#[derive(Serialize)]
struct ConverseRequest {
    messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<serde_json::Value>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ConverseMessage {
    role: String,
    content: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct InferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(rename = "stopSequences", skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

impl InferenceConfig {
    /// Whether any inference field is set (else the block is omitted entirely).
    fn is_empty(&self) -> bool {
        self.max_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.stop_sequences.is_empty()
    }
}

#[derive(Deserialize)]
struct ConverseResponse {
    #[serde(default)]
    output: ConverseOutput,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: ConverseUsage,
}

#[derive(Default, Deserialize)]
struct ConverseOutput {
    #[serde(default)]
    message: ConverseOutputMessage,
}

#[derive(Default, Deserialize)]
struct ConverseOutputMessage {
    #[serde(default)]
    content: Vec<ConverseContentBlock>,
}

#[derive(Deserialize)]
struct ConverseContentBlock {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "toolUse", default)]
    tool_use: Option<ConverseToolUse>,
}

#[derive(Deserialize)]
struct ConverseToolUse {
    #[serde(rename = "toolUseId", default)]
    tool_use_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Default, Deserialize)]
// Wire type mirroring the Converse `usage` object; the shared `tokens` suffix
// is the API's own naming.
#[allow(clippy::struct_field_names)]
struct ConverseUsage {
    #[serde(rename = "inputTokens", default)]
    input_tokens: u32,
    #[serde(rename = "outputTokens", default)]
    output_tokens: u32,
    #[serde(rename = "totalTokens", default)]
    total_tokens: Option<u32>,
}

/// Translate a Converse `stopReason` to an OpenAI `finish_reason`. Shared with
/// the streaming translator.
fn map_finish_reason(stop_reason: Option<&str>) -> Option<String> {
    match stop_reason {
        Some("end_turn" | "stop_sequence") => Some("stop".to_owned()),
        Some("max_tokens") => Some("length".to_owned()),
        Some("tool_use") => Some("tool_calls".to_owned()),
        Some("content_filtered" | "guardrail_intervened") => Some("content_filter".to_owned()),
        Some(other) => Some(other.to_owned()),
        None => None,
    }
}

/// Serialize a [`ChatRequest`] into the exact Converse request bytes to sign
/// and send.
fn serialize_body(req: &ChatRequest) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&translate_request(req))
        .map_err(|e| ProviderError::Translation(format!("bedrock request: {e}")))
}

/// Build the Converse request body from an OpenAI-shaped [`ChatRequest`]. The
/// model id is NOT part of the body (it is in the URL path).
fn translate_request(req: &ChatRequest) -> ConverseRequest {
    let mut system: Vec<serde_json::Value> = Vec::new();
    let mut messages: Vec<ConverseMessage> = Vec::new();

    for m in &req.messages {
        let text = m
            .content
            .as_ref()
            .map(|c| c.text().into_owned())
            .unwrap_or_default();
        match m.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system.push(json!({ "text": text }));
                }
            }
            "tool" => {
                let block = json!({
                    "toolResult": {
                        "toolUseId": m.extra.get("tool_call_id").cloned().unwrap_or_default(),
                        "content": [{ "text": text }],
                    }
                });
                match messages.last_mut() {
                    // Consecutive tool results merge into one user message
                    // (Converse expects strict user/assistant alternation). Only
                    // merge into a message that is ALREADY a tool-result carrier,
                    // never into a preceding plain user text message.
                    Some(prev)
                        if prev.role == "user"
                            && prev
                                .content
                                .last()
                                .is_some_and(|b| b.get("toolResult").is_some()) =>
                    {
                        prev.content.push(block);
                    }
                    _ => messages.push(ConverseMessage {
                        role: "user".to_owned(),
                        content: vec![block],
                    }),
                }
            }
            "assistant"
                if m.extra
                    .get("tool_calls")
                    .is_some_and(serde_json::Value::is_array) =>
            {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({ "text": text }));
                }
                if let Some(calls) = m.extra.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        content.push(json!({
                            "toolUse": {
                                "toolUseId": call.get("id").cloned().unwrap_or_default(),
                                "name": call.pointer("/function/name").cloned().unwrap_or_default(),
                                "input": parse_tool_arguments(call.pointer("/function/arguments")),
                            }
                        }));
                    }
                }
                messages.push(ConverseMessage {
                    role: "assistant".to_owned(),
                    content,
                });
            }
            role => messages.push(ConverseMessage {
                role: role.to_owned(),
                content: converse_content(m.content.as_ref(), &text),
            }),
        }
    }

    let inference = InferenceConfig {
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences: req
            .stop
            .as_ref()
            .map(collect_stop_sequences)
            .unwrap_or_default(),
    };

    ConverseRequest {
        messages,
        system,
        inference_config: if inference.is_empty() {
            None
        } else {
            Some(inference)
        },
        tool_config: translate_tool_config(req),
    }
}

/// Build a Converse message `content` array. Text-only messages become a single
/// `text` block; messages with images become interleaved `text`/`image` blocks.
/// Remote image URLs are dropped here (the gateway rejects them earlier via
/// [`ChatProvider::accepts_remote_image_url`] returning `false`).
fn converse_content(content: Option<&MessageContent>, text: &str) -> Vec<serde_json::Value> {
    match content {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    if let Some(block) = converse_image_block(img) {
                        blocks.push(block);
                    }
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        blocks.push(json!({ "text": t }));
                    }
                }
            }
            blocks
        }
        _ => vec![json!({ "text": text })],
    }
}

/// Translate one OpenAI `image_url` into a Converse `image` block. Only `data:`
/// URIs are supported (Converse takes raw image bytes, base64-encoded in JSON);
/// remote URLs return `None`.
fn converse_image_block(image: &ImageUrl) -> Option<serde_json::Value> {
    let data = image.as_data_uri()?;
    let format = match data.media_type.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(json!({
        "image": { "format": format, "source": { "bytes": data.base64_data } }
    }))
}

/// OpenAI tool-call `arguments` is a JSON *string*; Converse `input` is the
/// object itself. Unparseable arguments degrade to an empty object.
fn parse_tool_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    arguments
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}))
}

/// OpenAI `tools` + `tool_choice` -> Converse `toolConfig`, or `None` if the
/// request declares no tools.
fn translate_tool_config(req: &ChatRequest) -> Option<serde_json::Value> {
    let tools = req.extra.get("tools").and_then(|v| v.as_array())?;
    let specs: Vec<serde_json::Value> = tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "toolSpec": {
                    "name": function.get("name")?.as_str()?,
                    "description": function.get("description").and_then(|v| v.as_str()),
                    "inputSchema": {
                        "json": function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                    },
                }
            }))
        })
        .collect();
    if specs.is_empty() {
        return None;
    }
    let mut config = serde_json::Map::new();
    config.insert("tools".to_owned(), json!(specs));
    if let Some(choice) = req.extra.get("tool_choice").and_then(translate_tool_choice) {
        config.insert("toolChoice".to_owned(), choice);
    }
    Some(serde_json::Value::Object(config))
}

/// OpenAI `tool_choice` -> Converse `toolChoice`. Converse has no explicit
/// `none`; unsupported shapes are dropped (the upstream default applies).
fn translate_tool_choice(choice: &serde_json::Value) -> Option<serde_json::Value> {
    match choice {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "auto": {} })),
            "required" => Some(json!({ "any": {} })),
            _ => None,
        },
        serde_json::Value::Object(_) => choice
            .pointer("/function/name")
            .and_then(|v| v.as_str())
            .map(|name| json!({ "tool": { "name": name } })),
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

/// Build an OpenAI-shaped [`ChatResponse`] from a Converse response.
fn translate_response(resp: ConverseResponse, requested_model: &str) -> ChatResponse {
    let mut content = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    for block in resp.output.message.content {
        if let Some(text) = block.text {
            content.push_str(&text);
        }
        if let Some(tool) = block.tool_use {
            tool_calls.push(json!({
                "id": tool.tool_use_id,
                "type": "function",
                "function": {
                    "name": tool.name,
                    // OpenAI carries arguments as a JSON string.
                    "arguments": tool.input.unwrap_or_else(|| json!({})).to_string(),
                },
            }));
        }
    }

    let mut extra = serde_json::Map::new();
    if !tool_calls.is_empty() {
        extra.insert("tool_calls".to_owned(), json!(tool_calls));
    }
    // OpenAI uses `content: null` for pure tool-call messages.
    let content = if content.is_empty() && !extra.is_empty() {
        None
    } else {
        Some(MessageContent::Text(content))
    };

    let total = resp.usage.total_tokens.unwrap_or_else(|| {
        resp.usage
            .input_tokens
            .saturating_add(resp.usage.output_tokens)
    });
    let usage = Usage {
        prompt_tokens: resp.usage.input_tokens,
        completion_tokens: resp.usage.output_tokens,
        total_tokens: total,
        estimated: None,
        // Bedrock cache-read/write tokens are not surfaced yet (issue #99 scope).
        prompt_tokens_details: None,
        completion_tokens_details: None,
    };

    ChatResponse {
        id: generated_id(),
        object: "chat.completion".to_owned(),
        created: now_secs(),
        model: requested_model.to_owned(),
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

/// Current Unix time in seconds (0 if the clock is before the epoch).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Synthesize an OpenAI-style response id (Converse returns none of its own).
fn generated_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("chatcmpl-bedrock-{nanos}")
}

#[async_trait]
impl ChatProvider for BedrockProvider {
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
        let body = serialize_body(&req)?;
        let path = Self::path(&req.model, "converse");
        let bytes = self.send(&path, body, &cancel).await?;
        let parsed: ConverseResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("bedrock response: {e}")))?;
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

    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_sse_bytes(items))
    }

    /// Converse takes raw image BYTES only; it never fetches a remote URL. The
    /// gateway therefore rejects a remote image URL (LM-2004) rather than
    /// forwarding one Bedrock cannot use.
    fn accepts_remote_image_url(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{ContentPart, MessageContent};

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
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0".to_owned(),
            messages,
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
    fn model_id_is_percent_encoded_in_the_path() {
        let path = BedrockProvider::path("anthropic.claude-3-5-sonnet-20241022-v2:0", "converse");
        assert_eq!(
            path,
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
        );
    }

    #[test]
    fn region_parsed_from_standard_endpoint_and_none_for_custom() {
        assert_eq!(
            region_from_base_url(Some("https://bedrock-runtime.eu-west-3.amazonaws.com")),
            Some("eu-west-3".to_owned())
        );
        assert_eq!(region_from_base_url(Some("http://127.0.0.1:8080")), None);
        assert_eq!(region_from_base_url(None), None);
    }

    #[test]
    fn region_parsed_from_vpc_endpoint_shapes() {
        // Plain VPC endpoint host.
        assert_eq!(
            region_from_base_url(Some(
                "https://bedrock-runtime.us-gov-west-1.vpce.amazonaws.com"
            )),
            Some("us-gov-west-1".to_owned())
        );
        // PrivateLink DNS with a vpce-id prefix label.
        assert_eq!(
            region_from_base_url(Some(
                "https://vpce-0abc123-xyz.bedrock-runtime.ap-southeast-2.vpce.amazonaws.com"
            )),
            Some("ap-southeast-2".to_owned())
        );
        // A label after bedrock-runtime that is not region-shaped is rejected.
        assert_eq!(
            region_from_base_url(Some("https://bedrock-runtime.internal.example.com")),
            None
        );
    }

    #[test]
    fn region_shape_check_accepts_regions_and_rejects_noise() {
        assert!(looks_like_region("us-east-1"));
        assert!(looks_like_region("ap-southeast-2"));
        assert!(looks_like_region("us-gov-west-1"));
        assert!(!looks_like_region("amazonaws")); // no hyphen
        assert!(!looks_like_region("vpce-0abc")); // does not end with a digit
        assert!(!looks_like_region("Us-East-1")); // uppercase
        assert!(!looks_like_region("internal")); // no hyphen, no digit
    }

    #[test]
    fn default_endpoint_is_derived_from_region() {
        let p = BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock",
            "ap-southeast-2",
            None,
            None,
        );
        assert_eq!(
            p.endpoint,
            "https://bedrock-runtime.ap-southeast-2.amazonaws.com"
        );
        assert_eq!(p.host(), "bedrock-runtime.ap-southeast-2.amazonaws.com");
    }

    #[test]
    fn request_hoists_system_and_sets_inference_config() {
        let mut req = request(vec![
            msg("system", "be brief"),
            msg("user", "hi"),
            msg("system", "also polite"),
        ]);
        req.temperature = Some(0.5);
        req.max_tokens = Some(256);
        req.stop = Some(json!(["STOP"]));

        let out = serde_json::to_value(translate_request(&req)).unwrap();
        // Model id is NOT in the body.
        assert!(out.get("model").is_none());
        assert_eq!(
            out["system"],
            json!([{ "text": "be brief" }, { "text": "also polite" }])
        );
        assert_eq!(
            out["messages"],
            json!([{ "role": "user", "content": [{ "text": "hi" }] }])
        );
        assert_eq!(out["inferenceConfig"]["maxTokens"], 256);
        assert_eq!(out["inferenceConfig"]["temperature"], 0.5);
        assert_eq!(out["inferenceConfig"]["stopSequences"], json!(["STOP"]));
    }

    #[test]
    fn plain_request_omits_inference_and_tool_config() {
        let req = request(vec![msg("user", "hi")]);
        let out = serde_json::to_value(translate_request(&req)).unwrap();
        assert!(out.get("inferenceConfig").is_none());
        assert!(out.get("toolConfig").is_none());
        assert!(out.get("system").is_none());
    }

    /// Issue #72: in strict mode a field Converse cannot honor is an honest
    /// client rejection (`UnsupportedField` -> 400, LM-1001) BEFORE any
    /// signing or upstream call - the provider deliberately has no
    /// credentials, which would fail later with a different error.
    #[tokio::test]
    async fn strict_mode_rejects_unsupported_openai_fields_pre_flight() {
        use lumen_core::ChatProvider as _;
        use tokio_util::sync::CancellationToken;
        let provider = BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock-test",
            "us-east-1",
            Some("http://127.0.0.1:1".to_owned()),
            None,
        )
        .with_strict(true);

        for (field, value) in [
            ("response_format", json!({ "type": "json_object" })),
            ("seed", json!(42)),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(5)),
            ("logit_bias", json!({ "50256": -100 })),
            ("parallel_tool_calls", json!(false)),
            ("frequency_penalty", json!(0.5)),
            ("presence_penalty", json!(0.25)),
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
                        if provider == "bedrock-test" && f == field
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

    /// Lenient (default) mode drops the fields: the wire body never carries
    /// them (compile-time: the struct has no such fields).
    #[test]
    fn lenient_translation_emits_no_unsupported_fields_on_the_wire() {
        let mut req = request(vec![msg("user", "hi")]);
        req.extra.insert(
            "response_format".to_owned(),
            json!({ "type": "json_object" }),
        );
        req.extra.insert("seed".to_owned(), json!(42));
        req.extra.insert("logprobs".to_owned(), json!(true));
        req.extra
            .insert("parallel_tool_calls".to_owned(), json!(false));
        let out = serde_json::to_value(translate_request(&req)).unwrap();
        for field in UNSUPPORTED_CHAT_FIELDS {
            assert!(out.get(*field).is_none(), "{field} must not reach the wire");
        }
    }

    #[test]
    fn tools_and_tool_traffic_map_to_converse_shapes() {
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
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }
            }]),
        );
        extra.insert("tool_choice".to_owned(), json!("required"));

        let mut req = request(vec![
            msg("user", "weather in Paris?"),
            ChatMessage {
                role: "assistant".to_owned(),
                content: None,
                name: None,
                extra: assistant_extra,
            },
            ChatMessage {
                role: "tool".to_owned(),
                content: Some(MessageContent::Text("18C sunny".to_owned())),
                name: None,
                extra: tool_extra,
            },
        ]);
        req.extra = extra;

        let out = serde_json::to_value(translate_request(&req)).unwrap();
        assert_eq!(
            out["messages"][1]["content"][0]["toolUse"],
            json!({ "toolUseId": "call_1", "name": "get_weather", "input": { "city": "Paris" } })
        );
        assert_eq!(
            out["messages"][2]["content"][0]["toolResult"],
            json!({ "toolUseId": "call_1", "content": [{ "text": "18C sunny" }] })
        );
        assert_eq!(
            out["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
        assert_eq!(
            out["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["properties"]["city"]
                ["type"],
            "string"
        );
        assert_eq!(out["toolConfig"]["toolChoice"], json!({ "any": {} }));
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
        let req = request(vec![
            msg("user", "two lookups"),
            tool_msg("call_1", "a"),
            tool_msg("call_2", "b"),
        ]);
        let out = translate_request(&req);
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[1].content.len(), 2);
    }

    #[test]
    fn data_uri_image_becomes_a_bytes_source_block() {
        let req = request(vec![ChatMessage {
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
        }]);
        let out = serde_json::to_value(translate_request(&req)).unwrap();
        let blocks = &out["messages"][0]["content"];
        assert_eq!(blocks[0], json!({ "text": "describe" }));
        assert_eq!(
            blocks[1]["image"],
            json!({ "format": "png", "source": { "bytes": "AAAA" } })
        );
    }

    #[test]
    fn response_concatenates_text_and_maps_stop_reason_and_usage() {
        let resp: ConverseResponse = serde_json::from_value(json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "Hello " }, { "text": "world" }
            ] } },
            "stopReason": "max_tokens",
            "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 }
        }))
        .unwrap();
        let out = translate_response(resp, "anthropic.claude");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "anthropic.claude");
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
        assert!(!out.id.is_empty());
    }

    #[test]
    fn response_tool_use_becomes_openai_tool_calls() {
        let resp: ConverseResponse = serde_json::from_value(json!({
            "output": { "message": { "role": "assistant", "content": [
                { "toolUse": { "toolUseId": "tu_9", "name": "get_weather", "input": { "city": "Paris" } } }
            ] } },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 4, "outputTokens": 2 }
        }))
        .unwrap();
        let out = translate_response(resp, "m");
        let message = &out.choices[0].message;
        assert_eq!(message.content, None);
        let calls = message.extra["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "tu_9");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!({ "city": "Paris" }).to_string()
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        // usage without totalTokens is summed.
        assert_eq!(out.usage.unwrap().total_tokens, 6);
    }

    #[test]
    fn finish_reason_mapping_covers_known_values() {
        assert_eq!(map_finish_reason(Some("end_turn")).as_deref(), Some("stop"));
        assert_eq!(
            map_finish_reason(Some("tool_use")).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(
            map_finish_reason(Some("guardrail_intervened")).as_deref(),
            Some("content_filter")
        );
        assert_eq!(map_finish_reason(None), None);
    }

    #[test]
    fn provider_debug_never_reveals_secrets() {
        let creds = Credentials::new(
            "AKIAPUBLIC",
            "super-secret-key-value",
            Some("session-token-value".to_owned()),
        );
        let p = BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock",
            "us-east-1",
            None,
            Some(creds),
        );
        let dbg = format!("{p:?}");
        assert!(dbg.contains("AKIAPUBLIC"), "public id may show: {dbg}");
        assert!(
            !dbg.contains("super-secret-key-value"),
            "leaked secret: {dbg}"
        );
        assert!(!dbg.contains("session-token-value"), "leaked token: {dbg}");
    }

    #[tokio::test]
    async fn request_without_credentials_fails_cleanly() {
        let p = BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock",
            "us-east-1",
            Some("http://127.0.0.1:1".to_owned()),
            None,
        );
        let err = p
            .chat(request(vec![msg("user", "hi")]), CancellationToken::new())
            .await
            .expect_err("no creds should fail before sending");
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    /// The env credential source must never leak its secret override through
    /// `Debug` on the provider.
    #[test]
    fn env_credential_source_debug_never_reveals_the_override() {
        let p = BedrockProvider::new_with_env_credentials(
            reqwest::Client::new(),
            "bedrock",
            "us-east-1",
            None,
            Some("override-secret-value".to_owned()),
        );
        let dbg = format!("{p:?}");
        assert!(
            !dbg.contains("override-secret-value"),
            "leaked override: {dbg}"
        );
        assert!(dbg.contains("redacted"), "expected redaction marker: {dbg}");
    }

    /// Env-sourced credentials are resolved PER REQUEST, so a value written to
    /// the process environment after construction is picked up (the credential
    /// staleness mitigation). Uses dedicated env var reads via
    /// `Credentials::from_env`; only Bedrock code touches these variables.
    #[test]
    fn credentials_from_env_reads_vars_and_honours_secret_override() {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAENVTEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "env-secret");
        std::env::remove_var("AWS_SESSION_TOKEN");

        let creds = Credentials::from_env(None).expect("env creds resolve");
        assert_eq!(creds.access_key_id, "AKIAENVTEST");
        assert_eq!(creds.secret_access_key, "env-secret");
        assert!(creds.session_token.is_none());

        // The api_key override replaces only the secret half.
        let creds = Credentials::from_env(Some("override-secret".to_owned()))
            .expect("override creds resolve");
        assert_eq!(creds.access_key_id, "AKIAENVTEST");
        assert_eq!(creds.secret_access_key, "override-secret");

        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}
