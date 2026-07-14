//! Embedding types, mirroring the OpenAI `embeddings` schema.

use serde::{Deserialize, Deserializer, Serialize};

use crate::chat::ContentPart;

/// Input to an embedding request: a single string, a text batch, or a
/// multimodal batch of content-parts items.
///
/// (Token-array inputs are intentionally not modelled in v1.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedInput {
    /// A single piece of text (`"input": "hi"`).
    Single(String),
    /// A batch of texts (`"input": ["a","b"]`), embedded and returned in order.
    Batch(Vec<String>),
    /// A multimodal batch (`"input": ["a", [{parts}], ...]`): each item is a
    /// string or an array of content parts. Only entered when at least one item
    /// is a parts array (untagged order tries `Single`/`Batch` first).
    Multi(Vec<EmbedItem>),
}

/// One item of a multimodal embedding batch: a bare string or an array of
/// typed content parts (text and/or image), order preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedItem {
    /// A bare string item (tried first - untagged order matters).
    Text(String),
    /// An array of typed content parts.
    Parts(Vec<ContentPart>),
}

impl EmbedInput {
    /// Number of individual items in this input.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            EmbedInput::Single(_) => 1,
            EmbedInput::Batch(v) => v.len(),
            EmbedInput::Multi(v) => v.len(),
        }
    }

    /// Whether the input contains no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            EmbedInput::Single(_) => false,
            EmbedInput::Batch(v) => v.is_empty(),
            EmbedInput::Multi(v) => v.is_empty(),
        }
    }

    /// Whether any item carries an image part (dispatch by field presence).
    #[must_use]
    pub fn has_image(&self) -> bool {
        match self {
            EmbedInput::Single(_) | EmbedInput::Batch(_) => false,
            EmbedInput::Multi(items) => items.iter().any(|item| match item {
                EmbedItem::Text(_) => false,
                EmbedItem::Parts(parts) => parts.iter().any(|p| p.image().is_some()),
            }),
        }
    }

    /// Borrow the text inputs, regardless of shape. For text-only inputs this is
    /// one `&str` per item; for multimodal items it yields each text fragment
    /// (image parts contribute nothing). Used by the text-only provider paths
    /// and by token estimation.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.text_iter()
    }

    /// Every text fragment across all items (image parts contribute nothing).
    pub fn text_iter(&self) -> impl Iterator<Item = &str> {
        // Materialise into a `Vec` iterator to keep one concrete return type
        // across the three shapes without a bespoke iterator enum.
        let fragments: Vec<&str> = match self {
            EmbedInput::Single(s) => vec![s.as_str()],
            EmbedInput::Batch(v) => v.iter().map(String::as_str).collect(),
            EmbedInput::Multi(items) => items
                .iter()
                .flat_map(|item| -> Vec<&str> {
                    match item {
                        EmbedItem::Text(s) => vec![s.as_str()],
                        EmbedItem::Parts(parts) => {
                            parts.iter().filter_map(ContentPart::text_str).collect()
                        }
                    }
                })
                .collect(),
        };
        fragments.into_iter()
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
/// so a base64 request never breaks parsing (LUMEN always returns float
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
