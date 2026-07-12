//! Chat completion types, mirroring the OpenAI `chat/completions` schema.
//!
//! Unknown fields are preserved through [`serde(flatten)`] `extra` maps so that
//! provider-specific parameters pass through untouched — OpenAI compatibility
//! takes precedence over internal tidiness.

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
    pub content: Option<String>,
    /// Optional participant name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Any additional fields (`tool_calls`, `tool_call_id`, ...) preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
