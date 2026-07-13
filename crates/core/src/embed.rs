//! Embedding types, mirroring the OpenAI `embeddings` schema.

use serde::{Deserialize, Deserializer, Serialize};

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
    /// `Some(true)` when the gateway locally estimated the counts because the
    /// upstream reported none (ADR 003); omitted for upstream-reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
}

/// A single embedding vector with its position in the batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedData {
    /// Always `"embedding"`.
    pub object: String,
    pub index: u32,
    /// The vector. Deserializes from either a float array or OpenAI's base64
    /// form (little-endian f32 bytes); always serialized back as a float array.
    #[serde(deserialize_with = "deserialize_embedding")]
    pub embedding: Vec<f32>,
}

/// Deserialize an embedding as a float array or an OpenAI base64 string.
///
/// When `encoding_format: "base64"` is requested, OpenAI returns each embedding
/// as base64-encoded little-endian `f32` bytes. We decode it to a float vector
/// so a base64 request never breaks parsing (Ferrogate always returns float
/// arrays to the client in v1).
fn deserialize_embedding<'de, D>(deserializer: D) -> Result<Vec<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Floats(Vec<f32>),
        Base64(String),
    }

    match Repr::deserialize(deserializer)? {
        Repr::Floats(v) => Ok(v),
        Repr::Base64(s) => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(serde::de::Error::custom)?;
            if bytes.len() % 4 != 0 {
                return Err(serde::de::Error::custom(
                    "base64 embedding byte length is not a multiple of 4",
                ));
            }
            Ok(bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        }
    }
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
