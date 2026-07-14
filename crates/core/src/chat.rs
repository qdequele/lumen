//! Chat completion types, mirroring the OpenAI `chat/completions` schema.
//!
//! Unknown fields are preserved through [`serde(flatten)`] `extra` maps so that
//! provider-specific parameters pass through untouched - OpenAI compatibility
//! takes precedence over internal tidiness.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single chat message (request or response side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system`, `user`, `assistant`, `tool`, ...
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
    #[serde(rename = "type")]
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
    /// `text` parts for `Parts`). Empty for an image-only message. Used by the
    /// token estimator and any text-only inspection path.
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
    /// Whether the client requested a streaming (SSE) response.
    #[serde(default)]
    pub stream: bool,
    /// Any additional OpenAI fields (`tools`, `response_format`, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
}
