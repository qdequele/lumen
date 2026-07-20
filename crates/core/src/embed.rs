//! Embedding types, mirroring the OpenAI `embeddings` schema.
//!
//! Unknown request fields are captured into a [`serde(flatten)`] `extra` map
//! (the same idiom as [`crate::chat::ChatRequest`]) so provider translation
//! code can consume provider-specific parameters - e.g. Cohere's `input_type`
//! (search_query vs search_document, see `docs/providers.md` § cohere).
//! Unlike the chat path, `extra` is never re-serialized into an outgoing
//! provider body: unknown fields stop at the gateway (see the field docs on
//! [`EmbedRequest::extra`]).

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::chat::ContentPart;

/// Input to an embedding request: a single string, a text batch, a pre-tokenized
/// input (token ids), or a multimodal batch of content-parts items.
///
/// ## The item is the unit of embedding
///
/// Every provider produces exactly ONE embedding per input item, and the
/// gateway's automatic batching (`crates/providers/src/batch.rs`) splits by
/// item count, so [`len`](EmbedInput::len) is the authoritative count of
/// inner requests / embeddings a request yields. A [`Multi`](EmbedInput::Multi)
/// item that is a [`Parts`](EmbedItem::Parts) array is a SINGLE item: its text
/// parts are joined into one string (see [`item_texts`](EmbedInput::item_texts))
/// and image parts fuse into the same one embedding, exactly as the item-level
/// providers (jina, voyage, cohere) already treat them. Text-only providers
/// therefore issue one inner request per item (never one per text fragment),
/// which keeps the inner-request count at or below the provider's
/// `max_batch_size` after splitting (issue #90).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedInput {
    /// A single piece of text (`"input": "hi"`).
    Single(String),
    /// A batch of texts (`"input": ["a","b"]`), embedded and returned in order.
    Batch(Vec<String>),
    /// A single pre-tokenized input as an array of token ids
    /// (`"input": [1,2,3]`). Counts as one item (OpenAI semantics). Untagged
    /// order tries `Single`/`Batch` first, so this only matches an all-integer
    /// array.
    Tokens(Vec<u32>),
    /// A batch of pre-tokenized inputs (`"input": [[1,2],[3,4]]`), each an array
    /// of token ids, embedded and returned in order. Matches an array of
    /// all-integer arrays (tried before `Multi`, whose items are strings or
    /// content-part objects).
    TokenBatch(Vec<Vec<u32>>),
    /// A multimodal batch (`"input": ["a", [{parts}], ...]`): each item is a
    /// string or an array of content parts. Only entered when at least one item
    /// is a parts array (untagged order tries the text/token variants first).
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
    /// Number of individual items in this input, which equals the number of
    /// embeddings a provider returns and the number of inner requests it issues
    /// (one per item; see the type-level note). A single token array counts as
    /// one item (one embedding); a token batch counts each inner array; a
    /// [`Multi`](EmbedInput::Multi) `Parts` item counts as one regardless of how
    /// many parts it carries.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            EmbedInput::Single(_) | EmbedInput::Tokens(_) => 1,
            EmbedInput::Batch(v) => v.len(),
            EmbedInput::TokenBatch(v) => v.len(),
            EmbedInput::Multi(v) => v.len(),
        }
    }

    /// Whether the input contains no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            EmbedInput::Single(_) | EmbedInput::Tokens(_) => false,
            EmbedInput::Batch(v) => v.is_empty(),
            EmbedInput::TokenBatch(v) => v.is_empty(),
            EmbedInput::Multi(v) => v.is_empty(),
        }
    }

    /// Total number of token ids across pre-tokenized inputs (`0` for text and
    /// multimodal inputs). Used by the estimation fallback: one token id is one
    /// token, with no byte heuristic.
    #[must_use]
    pub fn token_count(&self) -> u64 {
        match self {
            EmbedInput::Tokens(ids) => ids.len() as u64,
            EmbedInput::TokenBatch(batches) => batches.iter().map(|ids| ids.len() as u64).sum(),
            EmbedInput::Single(_) | EmbedInput::Batch(_) | EmbedInput::Multi(_) => 0,
        }
    }

    /// Whether any item carries an image part (dispatch by field presence).
    #[must_use]
    pub fn has_image(&self) -> bool {
        match self {
            EmbedInput::Single(_)
            | EmbedInput::Batch(_)
            | EmbedInput::Tokens(_)
            | EmbedInput::TokenBatch(_) => false,
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
            // Pre-tokenized inputs carry no text; token counting handles them
            // via `token_count`.
            EmbedInput::Tokens(_) | EmbedInput::TokenBatch(_) => Vec::new(),
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

    /// The text of each item, ONE string per item, in order: the input a
    /// text-only provider (google, vertex, tei) sends as one inner request per
    /// item so the inner-request count equals [`len`](EmbedInput::len).
    ///
    /// A [`Multi`](EmbedInput::Multi) `Parts` item's text fragments are joined
    /// into a single string (image parts contribute nothing here; a text-only
    /// provider rejects image input before it reaches this method). The common
    /// single-fragment case borrows; only a genuine multi-fragment item
    /// allocates. Pre-tokenized inputs carry no text and yield nothing (those
    /// providers reject token-id input up front).
    #[must_use]
    pub fn item_texts(&self) -> Vec<Cow<'_, str>> {
        match self {
            EmbedInput::Single(s) => vec![Cow::Borrowed(s.as_str())],
            EmbedInput::Batch(v) => v.iter().map(|s| Cow::Borrowed(s.as_str())).collect(),
            EmbedInput::Tokens(_) | EmbedInput::TokenBatch(_) => Vec::new(),
            EmbedInput::Multi(items) => items.iter().map(EmbedItem::joined_text).collect(),
        }
    }
}

impl EmbedItem {
    /// This item's text as a single string (one embedding per item). A `Parts`
    /// item joins its text fragments; a single text fragment borrows, more than
    /// one allocates, and an item with no text fragment yields an empty string.
    fn joined_text(&self) -> Cow<'_, str> {
        match self {
            EmbedItem::Text(s) => Cow::Borrowed(s.as_str()),
            EmbedItem::Parts(parts) => {
                let mut texts = parts.iter().filter_map(ContentPart::text_str);
                match (texts.next(), texts.next()) {
                    (None, _) => Cow::Borrowed(""),
                    (Some(first), None) => Cow::Borrowed(first),
                    (Some(first), Some(second)) => {
                        let mut joined = String::with_capacity(first.len() + second.len());
                        joined.push_str(first);
                        joined.push_str(second);
                        for rest in texts {
                            joined.push_str(rest);
                        }
                        Cow::Owned(joined)
                    }
                }
            }
        }
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
    /// Any additional fields (e.g. Cohere's `input_type` override) captured
    /// verbatim on deserialization and available to provider translation code.
    ///
    /// `skip_serializing`: unlike `ChatRequest::extra`, this map is a
    /// gateway-side carrier only, NEVER re-serialized. The OpenAI-compatible
    /// near-passthrough providers (openai, mistral, jina, voyage) serialize
    /// the whole request as the outgoing body, and a strict upstream (vLLM
    /// etc.) may reject unknown fields; providers that consume an extra field
    /// (Cohere reads `input_type`) do so from the Rust field and write it into
    /// their own body struct.
    #[serde(flatten, skip_serializing)]
    pub extra: Map<String, Value>,
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

/// How an [`EmbedData`] vector is serialized back to the client. Mirrors the
/// OpenAI `encoding_format` request parameter (`"float"` / `"base64"`). Purely
/// an output concern: internally the vector is always `Vec<f32>` and this never
/// participates in deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbeddingEncoding {
    /// A JSON array of floats (the default).
    #[default]
    Float,
    /// A base64 string of little-endian `f32` bytes (OpenAI's `"base64"`).
    Base64,
}

/// A single embedding vector with its position in the batch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EmbedData {
    /// Always `"embedding"`.
    pub object: String,
    pub index: u32,
    /// The vector. Deserializes from either a float array or OpenAI's base64
    /// form (little-endian f32 bytes); held internally as `Vec<f32>`.
    #[serde(deserialize_with = "deserialize_embedding")]
    pub embedding: Vec<f32>,
    /// Output encoding chosen for the *client* response (set at the request
    /// edge from `encoding_format`). Never present on the wire IN: it defaults
    /// to [`EmbeddingEncoding::Float`] and is not deserialized.
    #[serde(default, skip_deserializing)]
    pub encoding: EmbeddingEncoding,
}

impl Serialize for EmbedData {
    /// Serialize `embedding` per the chosen [`EmbeddingEncoding`]: a float array
    /// by default, or an OpenAI-style base64 string of little-endian `f32` bytes
    /// when `encoding_format: "base64"` was requested.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("EmbedData", 3)?;
        st.serialize_field("object", &self.object)?;
        st.serialize_field("index", &self.index)?;
        match self.encoding {
            EmbeddingEncoding::Float => st.serialize_field("embedding", &self.embedding)?,
            EmbeddingEncoding::Base64 => {
                st.serialize_field("embedding", &encode_embedding_base64(&self.embedding))?;
            }
        }
        st.end()
    }
}

/// Encode an embedding as OpenAI's base64 form: the little-endian bytes of each
/// `f32`, concatenated, then standard base64. Inverse of the base64 branch of
/// [`deserialize_embedding`].
#[must_use]
pub fn encode_embedding_base64(embedding: &[f32]) -> String {
    use base64::Engine;
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for f in embedding {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_token_array_parses_as_tokens_one_item() {
        let input: EmbedInput = serde_json::from_str("[1,2,3]").expect("valid token array");
        assert!(matches!(input, EmbedInput::Tokens(_)));
        assert_eq!(input.len(), 1);
        assert!(!input.is_empty());
        assert_eq!(input.token_count(), 3);
        // No text fragments; the estimator relies on `token_count` instead.
        assert_eq!(input.text_iter().count(), 0);
    }

    #[test]
    fn nested_token_arrays_parse_as_token_batch() {
        let input: EmbedInput = serde_json::from_str("[[1,2],[3,4,5]]").expect("valid token batch");
        assert!(matches!(input, EmbedInput::TokenBatch(_)));
        assert_eq!(input.len(), 2);
        assert_eq!(input.token_count(), 5);
    }

    #[test]
    fn token_variants_round_trip_as_integer_arrays() {
        let req: EmbedRequest =
            serde_json::from_str(r#"{"model":"m","input":[1,2,3]}"#).expect("valid request");
        let back = serde_json::to_value(&req).expect("serialize");
        assert_eq!(back["input"], serde_json::json!([1, 2, 3]));

        let req: EmbedRequest =
            serde_json::from_str(r#"{"model":"m","input":[[1,2],[3,4]]}"#).expect("valid request");
        let back = serde_json::to_value(&req).expect("serialize");
        assert_eq!(back["input"], serde_json::json!([[1, 2], [3, 4]]));
    }

    #[test]
    fn string_batch_still_wins_over_tokens() {
        // Untagged order: an all-strings array stays `Batch`, empty stays `Batch`.
        let input: EmbedInput = serde_json::from_str(r#"["a","b"]"#).expect("valid batch");
        assert!(matches!(input, EmbedInput::Batch(_)));
        let empty: EmbedInput = serde_json::from_str("[]").expect("valid empty batch");
        assert!(matches!(empty, EmbedInput::Batch(_)));
        assert!(empty.is_empty());
    }

    #[test]
    fn multi_parts_item_is_one_unit_and_joins_its_text() {
        // Two items: a bare string, and a Parts item carrying two text
        // fragments. The item is the unit of embedding, so `len` is 2 and
        // `item_texts` yields two strings (the Parts fragments joined), NOT
        // three fragment-level entries (issue #90).
        let input = EmbedInput::Multi(vec![
            EmbedItem::Text("solo".to_owned()),
            EmbedItem::Parts(vec![
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("foo".to_owned()),
                    image_url: None,
                    extra: Map::new(),
                },
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("bar".to_owned()),
                    image_url: None,
                    extra: Map::new(),
                },
            ]),
        ]);

        assert_eq!(input.len(), 2);
        // Fragment-level iteration still sees all three fragments (token
        // estimation relies on it), but the per-item view collapses to one
        // string per item.
        assert_eq!(input.text_iter().count(), 3);

        let item_texts = input.item_texts();
        assert_eq!(item_texts.len(), 2);
        assert_eq!(item_texts[0], "solo");
        assert_eq!(item_texts[1], "foobar");
    }

    #[test]
    fn item_texts_matches_len_across_shapes() {
        // The invariant the batch path relies on: one text per item.
        let single = EmbedInput::Single("x".to_owned());
        assert_eq!(single.item_texts().len(), single.len());

        let batch = EmbedInput::Batch(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(batch.item_texts().len(), batch.len());
    }

    #[test]
    fn embed_data_serializes_as_float_array_by_default() {
        let data = EmbedData {
            object: "embedding".to_owned(),
            index: 0,
            embedding: vec![1.0, 2.0],
            encoding: EmbeddingEncoding::default(),
        };
        let v = serde_json::to_value(&data).expect("serialize");
        assert_eq!(v["embedding"], serde_json::json!([1.0, 2.0]));
    }

    #[test]
    fn embed_data_serializes_as_base64_when_requested() {
        let data = EmbedData {
            object: "embedding".to_owned(),
            index: 0,
            embedding: vec![1.0, 2.0],
            encoding: EmbeddingEncoding::Base64,
        };
        let v = serde_json::to_value(&data).expect("serialize");
        let b64 = v["embedding"].as_str().expect("base64 string");
        // Round-trips back through the base64 deserializer to the same floats.
        let json = format!(r#"{{"object":"embedding","index":0,"embedding":"{b64}"}}"#);
        let back: EmbedData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.embedding, vec![1.0, 2.0]);
    }

    #[test]
    fn base64_encode_is_inverse_of_decode() {
        let floats = vec![-0.5f32, 0.25, 1234.5];
        let b64 = encode_embedding_base64(&floats);
        let json = format!(r#"{{"object":"embedding","index":0,"embedding":"{b64}"}}"#);
        let back: EmbedData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.embedding, floats);
    }
}
