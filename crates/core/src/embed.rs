//! Embedding types, mirroring the OpenAI `embeddings` schema.

use serde::{Deserialize, Serialize};

/// Input to an embedding request: either a single string or a batch.
///
/// (Token-array inputs are intentionally not modelled in v1.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedInput {
    /// A single piece of text.
    Single(String),
    /// A batch of texts, embedded and returned in the same order.
    Batch(Vec<String>),
}

impl EmbedInput {
    /// Number of individual texts in this input.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            EmbedInput::Single(_) => 1,
            EmbedInput::Batch(v) => v.len(),
        }
    }

    /// Whether the input contains no texts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            EmbedInput::Single(_) => false,
            EmbedInput::Batch(v) => v.is_empty(),
        }
    }

    /// Borrow the inputs as a slice-like iterator, regardless of shape.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        // `Either`-free: collect into a small enum iterator.
        let slice: &[String] = match self {
            EmbedInput::Single(s) => std::slice::from_ref(s),
            EmbedInput::Batch(v) => v.as_slice(),
        };
        slice.iter().map(String::as_str)
    }
}

/// An embedding request in OpenAI format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub model: String,
    pub input: EmbedInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Token accounting for embeddings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EmbedUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// A single embedding vector with its position in the batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedData {
    /// Always `"embedding"`.
    pub object: String,
    pub index: u32,
    pub embedding: Vec<f32>,
}

/// An embedding response in OpenAI format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedResponse {
    /// Always `"list"`.
    pub object: String,
    pub data: Vec<EmbedData>,
    pub model: String,
    #[serde(default)]
    pub usage: EmbedUsage,
}
