//! Chat completion types, mirroring the OpenAI `chat/completions` schema.
//!
//! Unknown fields are preserved through [`serde(flatten)`] `extra` maps so that
//! provider-specific parameters pass through untouched - OpenAI compatibility
//! takes precedence over internal tidiness.

use std::borrow::Cow;

use bytes::Bytes;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single chat message (request or response side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system`, `developer`, `user`, `assistant`, `tool`, ...
    pub role: String,
    /// Textual content. `None` for e.g. assistant messages that only carry
    /// `tool_calls` (which live in `extra`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    /// Optional participant name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Any additional fields (`tool_calls`, `tool_call_id`, ...) preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// OpenAI's newer name for instructions. OpenAI treats `developer` and
/// `system` the same way (each is mapped to the other depending on the
/// model), and no other upstream has a `developer` role, so every translated
/// provider handles it as `system`.
pub const DEVELOPER_ROLE: &str = "developer";

impl ChatMessage {
    /// Whether this message carries instructions: role `system` or OpenAI's
    /// `developer`. Translators that hoist instructions out of the turn list
    /// (Anthropic, Gemini, Bedrock) or drop client instructions (virtual-model
    /// presets) must test this, never `role == "system"` alone.
    #[must_use]
    pub fn is_system_role(&self) -> bool {
        matches!(self.role.as_str(), "system" | DEVELOPER_ROLE)
    }
}

/// Rewrite every `developer` message to `system`, in place, for an
/// OpenAI-compatible upstream that has no `developer` role (Mistral, Azure
/// API versions before it, vLLM chat templates, ...). Position and content
/// are untouched, so the instructions keep their order.
pub fn developer_role_as_system(messages: &mut [ChatMessage]) {
    for m in messages.iter_mut().filter(|m| m.role == DEVELOPER_ROLE) {
        "system".clone_into(&mut m.role);
    }
}

/// Message content: OpenAI overloads this as either a bare string or an array
/// of typed parts (text and images). `untagged` so a JSON string deserializes
/// to `Text` and a JSON array to `Parts`; order matters (string tried first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// `"content": "hello"`.
    Text(String),
    /// `"content": [ {"type":"text",...}, {"type":"image_url",...} ]`.
    Parts(Vec<ContentPart>),
}

/// One element of a `Parts` array. A typed struct with a `flatten`ed `extra`
/// map (the codebase idiom) rather than an internally-tagged enum: serde
/// forbids an untagged catch-all variant inside `tag = "type"`, and unknown /
/// future part types (e.g. `input_audio`) must survive pass-through verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPart {
    /// The part discriminator: `"text"`, `"image_url"`, or a future type.
    /// Defaults to `"text"` when omitted, so `{"text":"hi"}` and
    /// `{"image_url":{...}}` parse without an explicit `type`; real
    /// OpenAI-shaped parts (which always send `type`) still parse.
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    /// Present when `kind == "text"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present when `kind == "image_url"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// Any other fields (and the payload of unknown part types), preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The default content-part `type` when none is given.
fn default_kind() -> String {
    "text".to_owned()
}

impl ContentPart {
    /// The image reference, if this part carries one. Dispatch is by field
    /// presence, not `kind` (since `kind` defaults to `"text"`).
    #[must_use]
    pub fn image(&self) -> Option<&ImageUrl> {
        self.image_url.as_ref()
    }

    /// Mutable access to the image reference, if any. Used by the embeddings
    /// image-fetch stage to rewrite a remote URL to an inline `data:` URI.
    pub fn image_mut(&mut self) -> Option<&mut ImageUrl> {
        self.image_url.as_mut()
    }

    /// The text of this part, if it is a text part. A part carrying an
    /// `image_url` is never treated as text even if it also has `text`.
    #[must_use]
    pub fn text_str(&self) -> Option<&str> {
        if self.image_url.is_some() {
            None
        } else {
            self.text.as_deref()
        }
    }
}

/// An `image_url` part's value: a URL (remote `http(s)` or a `data:` URI) plus
/// an optional `detail` hint. The gateway never dereferences `url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// A remote `http(s)://` URL or a `data:<media_type>;base64,<payload>` URI.
    pub url: String,
    /// Optional resolution hint (`"low"`/`"high"`/`"auto"`), forwarded untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A decoded `data:` URI: its media type and its base64 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUri {
    /// e.g. `image/png`.
    pub media_type: String,
    /// The base64 payload (still encoded - never decoded on the hot path).
    pub base64_data: String,
}

impl MessageContent {
    /// The concatenated text of the content (borrowed for `Text`; the joined
    /// `text` parts for `Parts`). Empty for an image-only message. Used by
    /// text-only inspection paths; the token estimator only needs
    /// [`text_len`](Self::text_len).
    #[must_use]
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            MessageContent::Text(s) => Cow::Borrowed(s),
            MessageContent::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    if p.kind == "text" {
                        if let Some(t) = &p.text {
                            out.push_str(t);
                        }
                    }
                }
                Cow::Owned(out)
            }
        }
    }

    /// The byte length of [`text`](Self::text), computed without building it
    /// (no allocation, whatever the number of parts).
    #[must_use]
    pub fn text_len(&self) -> usize {
        match self {
            MessageContent::Text(s) => s.len(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter(|p| p.kind == "text")
                .filter_map(|p| p.text.as_ref())
                .map(String::len)
                .sum(),
        }
    }

    /// Whether any part carries an image.
    #[must_use]
    pub fn has_image(&self) -> bool {
        matches!(self, MessageContent::Parts(parts) if parts.iter().any(|p| p.image_url.is_some()))
    }
}

impl ImageUrl {
    /// Parse a `data:<media_type>;base64,<payload>` URI, or `None` for any other
    /// (e.g. remote) URL. Only base64 data URIs are recognised.
    #[must_use]
    pub fn as_data_uri(&self) -> Option<DataUri> {
        let rest = self.url.strip_prefix("data:")?;
        let (media_type, payload) = rest.split_once(";base64,")?;
        if media_type.is_empty() || payload.is_empty() {
            return None;
        }
        Some(DataUri {
            media_type: media_type.to_owned(),
            base64_data: payload.to_owned(),
        })
    }

    /// Whether this is a remote `http(s)` URL (which the gateway forwards but
    /// never fetches).
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.url.starts_with("http://") || self.url.starts_with("https://")
    }

    /// A reference to a file already uploaded to Anthropic's Files API,
    /// spelled `anthropic-file:<file_id>` in the `url` field (never a real
    /// network scheme - the gateway forwards the id verbatim in a
    /// `source: {type: "file", file_id}` block and never dereferences it).
    #[must_use]
    pub fn anthropic_file_id(&self) -> Option<&str> {
        let id = self.url.strip_prefix("anthropic-file:")?;
        (!id.is_empty()).then_some(id)
    }

    /// A Gemini-native file reference: a Google Cloud Storage URI
    /// (`gs://bucket/object`) or a URI returned by the Gemini Files API
    /// (`https://generativelanguage.googleapis.com/...`). Both map straight
    /// onto Gemini's `fileData.fileUri`; the gateway never dereferences
    /// either. Note that the Gemini Developer API only resolves its own
    /// Files API URIs - `gs://` is a Vertex AI capability, forwarded
    /// verbatim and rejected by the default upstream (see the caveat in
    /// `docs/providers.md`).
    #[must_use]
    pub fn gemini_file_uri(&self) -> Option<&str> {
        if self.url.starts_with("gs://")
            || self
                .url
                .starts_with("https://generativelanguage.googleapis.com/")
        {
            Some(self.url.as_str())
        } else {
            None
        }
    }
}

/// A chat completion request in OpenAI format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Requested model id (as known to the gateway, before alias resolution).
    pub model: String,
    /// Conversation so far.
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    /// Whether the client requested a streaming (SSE) response. An explicit
    /// `null` reads as `false`, like an absent field.
    #[serde(default, deserialize_with = "null_as_false")]
    pub stream: bool,
    /// Any additional OpenAI fields (`tools`, `response_format`, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Deserialize a boolean where an explicit `null` means `false` (some SDKs
/// serialize an unset optional field as `null`).
fn null_as_false<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
}

impl ChatRequest {
    /// The output-token cap the client asked for.
    ///
    /// `max_tokens` wins when present. Otherwise falls back to
    /// `max_completion_tokens` (the field OpenAI reasoning models require
    /// instead of `max_tokens`), which lands in `extra`. A value that is not
    /// an unsigned integer fitting in `u32` is ignored. OpenAI-kind providers
    /// forward the body verbatim; every translating provider (Anthropic,
    /// Bedrock, Google, Cohere) must read the cap through this accessor.
    #[must_use]
    pub fn effective_max_tokens(&self) -> Option<u32> {
        self.max_tokens.or_else(|| {
            self.extra
                .get("max_completion_tokens")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        })
    }
}

/// Prompt-side token breakdown, an OpenAI-compatible `prompt_tokens_details`
/// object (issue #99). Every field is optional so an absent count is `None`
/// (omitted from the wire), never a fabricated zero (ADR 003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// Prompt tokens served from the provider's prompt cache (a cache *read*).
    /// OpenAI reports this directly; Anthropic's `cache_read_input_tokens`
    /// maps here (same read/hit semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Prompt tokens written to the provider's prompt cache on this request (a
    /// cache *write*). A Lumen extension carrying Anthropic's
    /// `cache_creation_input_tokens`, which has no OpenAI equivalent; kept
    /// distinct from `cached_tokens` so a write is never reported as a read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
}

/// Completion-side token breakdown, an OpenAI-compatible
/// `completion_tokens_details` object (issue #99).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    /// Reasoning tokens billed within the completion (OpenAI o-series and
    /// compatible models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

/// Token accounting returned by upstream providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// `Some(true)` when the gateway locally estimated the counts because the
    /// upstream reported none (ADR 003); omitted for upstream-reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
    /// Prompt-side breakdown (cached / cache-creation tokens). `None` when the
    /// upstream reported no breakdown; never fabricated (ADR 003, issue #99).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Completion-side breakdown (reasoning tokens). `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

impl Usage {
    /// Cached (cache-read) prompt tokens, if the upstream reported them.
    #[must_use]
    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details.and_then(|d| d.cached_tokens)
    }

    /// Cache-creation (cache-write) prompt tokens, if reported (Anthropic).
    #[must_use]
    pub fn cache_write_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details
            .and_then(|d| d.cache_creation_tokens)
    }

    /// Reasoning tokens billed within the completion, if reported.
    #[must_use]
    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .and_then(|d| d.reasoning_tokens)
    }
}

/// One completion choice in a non-streaming response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// A non-streaming chat completion response in OpenAI format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    /// Always `"chat.completion"`.
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A non-streaming completion as a provider hands it to the gateway: the typed
/// response, or the upstream body itself when it can be forwarded unchanged
/// (ADR 004, 2026-11-12 amendment).
#[derive(Debug, Clone, PartialEq)]
pub enum ChatCompletion {
    /// A response the gateway may read and modify (translating providers, and
    /// any body whose usage has to be estimated locally).
    Typed(ChatResponse),
    /// An OpenAI-format upstream body to forward byte for byte.
    Passthrough(ChatPassthrough),
}

/// An OpenAI-format completion body forwarded verbatim, with the usage the
/// gateway accounts for. Only [`ChatCompletion::from_openai_body`] builds one,
/// so the usage is always upstream-reported and non-zero (ADR 003).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatPassthrough {
    body: Bytes,
    usage: Usage,
}

impl ChatPassthrough {
    /// The upstream-reported usage (never all-zero).
    #[must_use]
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// The upstream body, to send to the client unchanged.
    #[must_use]
    pub fn into_body(self) -> Bytes {
        self.body
    }
}

/// The only parts of an OpenAI completion body the gateway reads when it
/// forwards the body unchanged. Every other field is skipped without being
/// built (no allocation), though the whole body is still checked to be JSON.
///
/// Read through [`ObjectOnly`], like its `usage`: a derived struct also
/// deserializes from a JSON array, which no completion is.
#[derive(Deserialize)]
struct CompletionProbe {
    #[serde(default)]
    usage: Option<ObjectOnly<Usage>>,
    #[serde(default)]
    choices: Option<SkippedArray>,
}

/// `T` read from a JSON object and from nothing else.
///
/// serde's derived `Deserialize` also accepts a JSON array, taking the
/// struct's fields by position, so `[{"prompt_tokens":8},[]]` would read as a
/// struct with a `usage` and a `choices` field. The probes decide whether an
/// upstream body is forwarded and billed as it stands, so they read every
/// struct through this wrapper: an array in place of an object fails the
/// probe and the body goes to the typed parser instead.
pub(crate) struct ObjectOnly<T>(pub(crate) T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectOnly<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = ObjectOnly<T>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(ObjectOnly)
            }
        }

        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

/// A JSON array whose elements are skipped, not built.
struct SkippedArray;

impl<'de> Deserialize<'de> for SkippedArray {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SkippedArray;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(SkippedArray)
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

impl ChatCompletion {
    /// Read a non-streaming OpenAI-format completion body.
    ///
    /// When the body reports usable usage (a prompt or completion count above
    /// zero) and has a `choices` array, nothing in it has to change: it is
    /// kept as bytes and only `usage` is read. Otherwise (no usage to trust,
    /// or a body the probe cannot read) it is parsed into a [`ChatResponse`]
    /// so the gateway can add its flagged estimate (ADR 003).
    ///
    /// # Errors
    /// The body is not a valid OpenAI chat completion.
    pub fn from_openai_body(body: Bytes) -> Result<Self, serde_json::Error> {
        if let Ok(ObjectOnly(CompletionProbe {
            usage: Some(ObjectOnly(usage)),
            choices: Some(SkippedArray),
        })) = serde_json::from_slice::<ObjectOnly<CompletionProbe>>(&body)
        {
            if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
                return Ok(Self::Passthrough(ChatPassthrough { body, usage }));
            }
        }
        serde_json::from_slice::<ChatResponse>(&body).map(Self::Typed)
    }
}

/// The incremental delta carried by a streaming chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChatDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One choice within a streaming chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatChunkChoice {
    pub index: u32,
    pub delta: ChatDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// A single streaming chunk (`chat.completion.chunk`) as emitted over SSE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChunk {
    pub id: String,
    /// Always `"chat.completion.chunk"`.
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: Some(MessageContent::Text(role.to_owned())),
            name: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn developer_and_system_are_system_roles_and_nothing_else_is() {
        assert!(msg("system").is_system_role());
        assert!(msg("developer").is_system_role());
        for role in ["user", "assistant", "tool", "Developer", ""] {
            assert!(!msg(role).is_system_role(), "{role}");
        }
    }

    #[test]
    fn developer_role_as_system_rewrites_only_developer_in_place() {
        let mut messages = vec![
            msg("system"),
            msg("developer"),
            msg("user"),
            msg("developer"),
        ];
        developer_role_as_system(&mut messages);
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "system", "user", "system"]);
        // Content and order are preserved.
        let texts: Vec<String> = messages
            .iter()
            .map(|m| {
                m.content
                    .as_ref()
                    .map(|c| c.text().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(texts, ["system", "developer", "user", "developer"]);
    }

    #[test]
    fn string_content_round_trips() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(m.content, Some(MessageContent::Text(ref s)) if s == "hello"));
        // Re-serializes back to a bare string (OpenAI compatibility).
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["content"], "hello");
    }

    #[test]
    fn parts_with_image_round_trip_and_are_detected() {
        let json = r#"{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        let content = m.content.as_ref().unwrap();
        assert!(content.has_image());
        assert_eq!(content.text(), "what is this?");
        // image_url survives round-trip.
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(
            out["content"][1]["image_url"]["url"],
            "https://example.com/cat.png"
        );
    }

    #[test]
    fn unknown_part_type_survives_round_trip() {
        let json = r#"{"role":"user","content":[
            {"type":"input_audio","input_audio":{"data":"AAAA","format":"wav"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        let content = m.content.as_ref().unwrap();
        assert!(!content.has_image());
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["content"][0]["type"], "input_audio");
        assert_eq!(out["content"][0]["input_audio"]["format"], "wav");
    }

    #[test]
    fn data_uri_is_parsed_and_remote_is_detected() {
        let inline = ImageUrl {
            url: "data:image/png;base64,iVBORw0KGgo=".to_owned(),
            detail: None,
        };
        let parsed = inline.as_data_uri().unwrap();
        assert_eq!(parsed.media_type, "image/png");
        assert_eq!(parsed.base64_data, "iVBORw0KGgo=");
        assert!(!inline.is_remote());

        let remote = ImageUrl {
            url: "https://example.com/x.png".to_owned(),
            detail: None,
        };
        assert!(remote.as_data_uri().is_none());
        assert!(remote.is_remote());
    }

    #[test]
    fn image_only_message_has_empty_text() {
        let json = r#"{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.content.as_ref().unwrap().text(), "");
    }

    #[test]
    fn text_len_is_the_length_of_text_for_every_shape() {
        for json in [
            r#"{"role":"user","content":"héllo"}"#,
            r#"{"role":"user","content":[
                {"type":"text","text":"what is "},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},
                {"type":"input_audio","text":"not a text part"},
                {"type":"text","text":"this?"},
                {"type":"text"}
            ]}"#,
            r#"{"role":"user","content":[]}"#,
        ] {
            let m: ChatMessage = serde_json::from_str(json).unwrap();
            let content = m.content.as_ref().unwrap();
            assert_eq!(content.text_len(), content.text().len(), "{json}");
        }
    }

    #[test]
    fn stream_null_reads_as_false() {
        // Some SDKs serialize an unset `stream` as an explicit null.
        let parse = |stream: &str| {
            serde_json::from_str::<ChatRequest>(&format!(
                r#"{{"model":"m","messages":[]{stream}}}"#
            ))
        };
        assert!(!parse(r#","stream":null"#).unwrap().stream);
        assert!(!parse("").unwrap().stream);
        assert!(!parse(r#","stream":false"#).unwrap().stream);
        assert!(parse(r#","stream":true"#).unwrap().stream);
        // Still a boolean: anything else is a client error.
        assert!(parse(r#","stream":"yes""#).is_err());
        assert!(parse(r#","stream":1"#).is_err());
    }

    #[test]
    fn anthropic_file_id_is_recognised_and_others_are_not() {
        let file = ImageUrl {
            url: "anthropic-file:file_011CNvxvfvyGnGnDtjPtzY9J".to_owned(),
            detail: None,
        };
        assert_eq!(
            file.anthropic_file_id(),
            Some("file_011CNvxvfvyGnGnDtjPtzY9J")
        );
        assert!(file.gemini_file_uri().is_none());

        // An empty id after the scheme is not a valid reference.
        let empty = ImageUrl {
            url: "anthropic-file:".to_owned(),
            detail: None,
        };
        assert!(empty.anthropic_file_id().is_none());

        // A plain data URI or remote URL never parses as a file id.
        let data = ImageUrl {
            url: "data:image/png;base64,AAAA".to_owned(),
            detail: None,
        };
        assert!(data.anthropic_file_id().is_none());
    }

    #[test]
    fn usage_without_breakdown_omits_detail_objects() {
        // Three-field usage (the common case) is unchanged: no `*_details`
        // keys leak into the serialized body, and the accessors report None.
        let json = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.cached_tokens(), None);
        assert_eq!(usage.reasoning_tokens(), None);
        assert_eq!(usage.cache_write_tokens(), None);
        let out = serde_json::to_value(usage).unwrap();
        assert!(out.get("prompt_tokens_details").is_none());
        assert!(out.get("completion_tokens_details").is_none());
    }

    #[test]
    fn usage_deserializes_openai_shaped_breakdown_and_round_trips() {
        // OpenAI reports the breakdown nested inside `prompt_tokens_details`
        // and `completion_tokens_details`; both must survive deserialization
        // and re-serialize in the same OpenAI-compatible shape.
        let json = r#"{
            "prompt_tokens":100,"completion_tokens":40,"total_tokens":140,
            "prompt_tokens_details":{"cached_tokens":64},
            "completion_tokens_details":{"reasoning_tokens":30}
        }"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cached_tokens(), Some(64));
        assert_eq!(usage.reasoning_tokens(), Some(30));
        assert_eq!(usage.cache_write_tokens(), None);
        let out = serde_json::to_value(usage).unwrap();
        assert_eq!(out["prompt_tokens_details"]["cached_tokens"], 64);
        assert_eq!(out["completion_tokens_details"]["reasoning_tokens"], 30);
    }

    #[test]
    fn usage_cache_write_tokens_serialize_inside_prompt_details() {
        // Anthropic's cache-creation (write) count has no OpenAI slot; it
        // rides alongside `cached_tokens` in `prompt_tokens_details` and is
        // omitted when absent.
        let usage = Usage {
            prompt_tokens: 8,
            completion_tokens: 2,
            total_tokens: 10,
            estimated: None,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(5),
                cache_creation_tokens: Some(3),
            }),
            completion_tokens_details: None,
        };
        assert_eq!(usage.cached_tokens(), Some(5));
        assert_eq!(usage.cache_write_tokens(), Some(3));
        let out = serde_json::to_value(usage).unwrap();
        assert_eq!(out["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(out["prompt_tokens_details"]["cache_creation_tokens"], 3);
        assert!(out.get("completion_tokens_details").is_none());
    }

    #[test]
    fn gemini_file_uri_recognises_gcs_and_files_api_uris() {
        let gcs = ImageUrl {
            url: "gs://my-bucket/cat.png".to_owned(),
            detail: None,
        };
        assert_eq!(gcs.gemini_file_uri(), Some("gs://my-bucket/cat.png"));
        assert!(gcs.anthropic_file_id().is_none());

        let files_api = ImageUrl {
            url: "https://generativelanguage.googleapis.com/v1beta/files/abc-123".to_owned(),
            detail: None,
        };
        assert_eq!(
            files_api.gemini_file_uri(),
            Some("https://generativelanguage.googleapis.com/v1beta/files/abc-123")
        );

        // A generic remote URL (even https) is not a Gemini-native reference.
        let remote = ImageUrl {
            url: "https://example.com/cat.png".to_owned(),
            detail: None,
        };
        assert!(remote.gemini_file_uri().is_none());
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            model: "m".to_owned(),
            messages: vec![msg("user")],
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
    fn effective_max_tokens_prefers_max_tokens() {
        let mut req = chat_request();
        req.max_tokens = Some(100);
        req.extra
            .insert("max_completion_tokens".into(), serde_json::json!(500));
        assert_eq!(req.effective_max_tokens(), Some(100));
    }

    #[test]
    fn effective_max_tokens_falls_back_to_max_completion_tokens() {
        let mut req = chat_request();
        assert_eq!(req.effective_max_tokens(), None);
        req.extra
            .insert("max_completion_tokens".into(), serde_json::json!(500));
        assert_eq!(req.effective_max_tokens(), Some(500));
        req.extra
            .insert("max_completion_tokens".into(), serde_json::json!(0));
        assert_eq!(req.effective_max_tokens(), Some(0));
    }

    #[test]
    fn effective_max_tokens_ignores_non_integers() {
        let mut req = chat_request();
        for bad in [
            serde_json::json!("500"),
            serde_json::json!(1.5),
            serde_json::json!(-1),
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            req.extra.insert("max_completion_tokens".into(), bad);
            assert_eq!(req.effective_max_tokens(), None);
        }
    }

    /// An upstream body with odd spacing, an unknown top-level field and an
    /// unknown usage field: none of it survives a typed round trip unchanged.
    const UPSTREAM_BODY: &str = r#"{ "id":"c-1","object":"chat.completion","created":7,
  "model":"gpt-4o", "system_fingerprint":"fp_1",
  "choices":[{"index":0,"message":{"role":"assistant","content":"hi [\"x\"] }"},"logprobs":null,"finish_reason":"stop"}],
  "usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"cost":0.25,
           "prompt_tokens_details":{"cached_tokens":4}} }"#;

    #[test]
    fn a_body_with_reported_usage_is_kept_as_the_bytes_received() {
        let body = Bytes::from_static(UPSTREAM_BODY.as_bytes());
        let ChatCompletion::Passthrough(upstream) =
            ChatCompletion::from_openai_body(body.clone()).unwrap()
        else {
            panic!("expected a passthrough");
        };
        assert_eq!(upstream.usage().prompt_tokens, 8);
        assert_eq!(upstream.usage().completion_tokens, 2);
        assert_eq!(upstream.usage().cached_tokens(), Some(4));
        assert_eq!(upstream.into_body(), body);
    }

    #[test]
    fn a_body_without_usable_usage_is_parsed_for_the_local_estimate() {
        for usage in [
            "",
            r#","usage":null"#,
            r#","usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}"#,
        ] {
            let body = format!(
                r#"{{"id":"c","object":"chat.completion","created":1,"model":"m",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"hi"}}}}]{usage}}}"#
            );
            let completion = ChatCompletion::from_openai_body(Bytes::from(body)).unwrap();
            assert!(
                matches!(completion, ChatCompletion::Typed(_)),
                "usage fragment {usage:?}"
            );
        }
    }

    #[test]
    fn a_usage_that_is_not_an_object_is_never_forwarded_verbatim() {
        // serde reads a derived struct from an array too (fields by
        // position). Such a body must not be forwarded as it stands.
        let body = r#"{"id":"c","object":"chat.completion","created":1,"model":"m",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"}}],
            "usage":[8,2,10]}"#;
        let completion = ChatCompletion::from_openai_body(Bytes::from_static(body.as_bytes()));
        assert!(
            !matches!(completion, Ok(ChatCompletion::Passthrough(_))),
            "{completion:?}"
        );
    }

    #[test]
    fn a_body_that_is_not_a_completion_is_an_error_even_with_usage() {
        for body in [
            // No `choices`: usage alone does not make a completion.
            r#"{"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
            // `choices` of the wrong type.
            r#"{"choices":"x","usage":{"prompt_tokens":8,"completion_tokens":2}}"#,
            // Truncated after a complete usage object.
            r#"{"usage":{"prompt_tokens":8,"completion_tokens":2},"choices":[{"index":0"#,
            // A top-level array is never a completion, even when its items
            // line up with the fields the probe reads.
            r#"[{"prompt_tokens":8},[]]"#,
            r#"[{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10},[{"index":0}]]"#,
            "[]",
            "not json",
        ] {
            assert!(
                ChatCompletion::from_openai_body(Bytes::from_static(body.as_bytes())).is_err(),
                "{body}"
            );
        }
    }
}
