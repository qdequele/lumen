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

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::chat::{ContentPart, ObjectOnly};

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
    ///
    /// Fragments are joined with a single newline, NOT concatenated directly:
    /// distinct text chunks are separate pieces of content, and an empty
    /// separator would fuse the trailing and leading tokens across a boundary
    /// (`"brown" + "fox"` becoming `"brownfox"`), degrading the embedding. A
    /// newline is the conventional separator for distinct text chunks and
    /// cannot merge adjacent tokens.
    ///
    /// Public so every text-only provider (google, vertex, tei, jina) shares
    /// one join, rather than each re-deriving it (and risking the empty-concat
    /// bug independently).
    #[must_use]
    pub fn joined_text(&self) -> Cow<'_, str> {
        match self {
            EmbedItem::Text(s) => Cow::Borrowed(s.as_str()),
            EmbedItem::Parts(parts) => {
                let mut texts = parts.iter().filter_map(ContentPart::text_str);
                match (texts.next(), texts.next()) {
                    (None, _) => Cow::Borrowed(""),
                    (Some(first), None) => Cow::Borrowed(first),
                    (Some(first), Some(second)) => {
                        // +2 for the newline separators between the first three
                        // fragments; further fragments grow it as needed.
                        let mut joined = String::with_capacity(first.len() + second.len() + 2);
                        joined.push_str(first);
                        joined.push('\n');
                        joined.push_str(second);
                        for rest in texts {
                            joined.push('\n');
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

impl EmbedRequest {
    /// The encoding the vectors of this request's answer are returned in:
    /// base64 for `encoding_format: "base64"`, a float array for any other
    /// value or none.
    #[must_use]
    pub fn output_encoding(&self) -> EmbeddingEncoding {
        if self.encoding_format.as_deref() == Some("base64") {
            EmbeddingEncoding::Base64
        } else {
            EmbeddingEncoding::Float
        }
    }
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
///
/// A hand-written visitor rather than a `#[serde(untagged)]` enum: an untagged
/// enum first buffers the whole value into serde's generic `Content` tree
/// (one boxed-enum node per float) and only then converts it, which doubles
/// the work and the allocations for every vector of every embedding response.
/// The visitor reads the array straight into the `Vec<f32>`.
fn deserialize_embedding<'de, D>(deserializer: D) -> Result<Vec<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    struct EmbeddingVisitor;

    impl<'de> serde::de::Visitor<'de> for EmbeddingVisitor {
        type Value = Vec<f32>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an array of numbers or a base64 string")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            // The hint is upstream-controlled: cap it so a lying one cannot
            // force a large allocation up front.
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
            while let Some(value) = seq.next_element::<f32>()? {
                out.push(value);
            }
            Ok(out)
        }

        fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(E::custom)?;
            if bytes.len() % 4 != 0 {
                return Err(E::custom(
                    "base64 embedding byte length is not a multiple of 4",
                ));
            }
            Ok(bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect())
        }
    }

    deserializer.deserialize_any(EmbeddingVisitor)
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

/// An embeddings answer as a provider hands it to the gateway: the typed
/// response, or the upstream body itself when it can be forwarded unchanged
/// (ADR 004, 2026-11-12 amendment).
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedCompletion {
    /// Vectors decoded to `Vec<f32>`, for the gateway to re-encode, reorder
    /// or annotate.
    Typed(EmbedResponse),
    /// An OpenAI-format upstream body to forward byte for byte.
    Passthrough(EmbedPassthrough),
}

/// An OpenAI-format embeddings body forwarded verbatim. Only
/// [`EmbedCompletion::from_openai_body`] builds one, so its usage is always
/// upstream-reported and non-zero (ADR 003) and its vectors are all in the
/// encoding the client asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedPassthrough {
    body: Bytes,
    usage: EmbedUsage,
    vectors: usize,
}

impl EmbedPassthrough {
    /// The upstream-reported usage (`prompt_tokens` is never zero).
    #[must_use]
    pub fn usage(&self) -> &EmbedUsage {
        &self.usage
    }

    /// The upstream body, to send to the client unchanged.
    #[must_use]
    pub fn into_body(self) -> Bytes {
        self.body
    }
}

/// The only parts of an OpenAI embeddings body the gateway reads when it
/// forwards the body unchanged: the usage, and how many vectors `data` holds
/// in which encoding. No vector is decoded or allocated.
///
/// Read through [`ObjectOnly`], like its `usage` and each `data` item: a
/// derived struct also deserializes from a JSON array, which none of them is.
#[derive(Deserialize)]
struct EmbedProbe {
    data: DataProbe,
    #[serde(default)]
    usage: Option<ObjectOnly<EmbedUsage>>,
}

/// The `data` array of an embeddings body, counted without decoding it.
struct DataProbe {
    /// How many items the array holds.
    vectors: usize,
    /// How many of them are float arrays (the rest are base64 strings).
    floats: usize,
}

impl<'de> Deserialize<'de> for DataProbe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// One `data` item: only the JSON type of its vector matters.
        #[derive(Deserialize)]
        struct Item {
            embedding: VectorKind,
            /// Required, as in [`EmbedData`]; the value is not used.
            #[serde(rename = "index")]
            _index: u32,
        }

        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DataProbe;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of embedding objects")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut probe = DataProbe {
                    vectors: 0,
                    floats: 0,
                };
                while let Some(ObjectOnly(item)) = seq.next_element::<ObjectOnly<Item>>()? {
                    probe.vectors += 1;
                    if item.embedding == VectorKind(EmbeddingEncoding::Float) {
                        probe.floats += 1;
                    }
                }
                Ok(probe)
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

/// The wire encoding of one vector, read off its JSON type: an array is a
/// float vector, a string is base64. The contents are skipped, not decoded.
#[derive(PartialEq, Eq)]
struct VectorKind(EmbeddingEncoding);

impl<'de> Deserialize<'de> for VectorKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = VectorKind;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of numbers or a base64 string")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(VectorKind(EmbeddingEncoding::Float))
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(VectorKind(EmbeddingEncoding::Base64))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl EmbedCompletion {
    /// Read an OpenAI-format embeddings body for a client that asked for
    /// vectors in the `wanted` encoding.
    ///
    /// When the body reports usage (`prompt_tokens` above zero) and every
    /// vector is already in the `wanted` encoding, nothing in it has to
    /// change: it is kept as bytes, and only the usage and the vector count
    /// are read. Otherwise (no usage to trust, vectors to re-encode, or a body
    /// the probe cannot read) it is parsed into an [`EmbedResponse`].
    ///
    /// # Errors
    /// The body is not a valid OpenAI embeddings response.
    pub fn from_openai_body(
        body: Bytes,
        wanted: EmbeddingEncoding,
    ) -> Result<Self, serde_json::Error> {
        if let Ok(ObjectOnly(EmbedProbe {
            data,
            usage: Some(ObjectOnly(usage)),
        })) = serde_json::from_slice::<ObjectOnly<EmbedProbe>>(&body)
        {
            let wanted_floats = match wanted {
                EmbeddingEncoding::Float => data.vectors,
                EmbeddingEncoding::Base64 => 0,
            };
            if usage.prompt_tokens > 0 && data.floats == wanted_floats {
                return Ok(Self::Passthrough(EmbedPassthrough {
                    body,
                    usage,
                    vectors: data.vectors,
                }));
            }
        }
        serde_json::from_slice::<EmbedResponse>(&body).map(Self::Typed)
    }

    /// How many vectors the answer holds, to check against the number of
    /// inputs sent (a short upstream response is never accepted, issue #89).
    #[must_use]
    pub fn vector_count(&self) -> usize {
        match self {
            Self::Typed(response) => response.data.len(),
            Self::Passthrough(upstream) => upstream.vectors,
        }
    }
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
        // Fragments join with a newline, never an empty separator (which would
        // fuse tokens across the boundary).
        assert_eq!(item_texts[1], "foo\nbar");
    }

    #[test]
    fn multi_text_parts_join_with_newline_not_empty_separator() {
        // Two distinct text chunks must not fuse at their boundary
        // ("brown" + "fox" -> "brownfox"); a newline keeps them apart.
        let joined = EmbedInput::Multi(vec![EmbedItem::Parts(vec![
            ContentPart {
                kind: "text".to_owned(),
                text: Some("The quick brown".to_owned()),
                image_url: None,
                extra: Map::new(),
            },
            ContentPart {
                kind: "text".to_owned(),
                text: Some("fox jumps".to_owned()),
                image_url: None,
                extra: Map::new(),
            },
        ])]);
        let texts = joined.item_texts();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0], "The quick brown\nfox jumps");

        // A single text part is unchanged (a genuine borrow, no separator).
        let single = EmbedInput::Multi(vec![EmbedItem::Parts(vec![ContentPart {
            kind: "text".to_owned(),
            text: Some("The quick brown".to_owned()),
            image_url: None,
            extra: Map::new(),
        }])]);
        assert_eq!(single.item_texts()[0], "The quick brown");
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

    fn embed_data_from(embedding_json: &str) -> Result<EmbedData, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"object":"embedding","index":0,"embedding":{embedding_json}}}"#
        ))
    }

    #[test]
    fn embedding_deserializes_from_a_number_array() {
        // Floats, integers, negatives and exponents are all JSON numbers.
        let data = embed_data_from("[0.5, -1, 2, 1e-3]").expect("float array");
        assert_eq!(data.embedding, vec![0.5, -1.0, 2.0, 1e-3]);
        assert_eq!(data.encoding, EmbeddingEncoding::Float);
        // An empty array is an empty vector.
        assert_eq!(embed_data_from("[]").expect("empty").embedding.len(), 0);
        // Through a `Value` (owned, non-streaming deserializer) too.
        let via_value: EmbedData = serde_json::from_value(
            serde_json::json!({"object":"embedding","index":3,"embedding":[1.5, 2]}),
        )
        .expect("from value");
        assert_eq!(via_value.embedding, vec![1.5, 2.0]);
        assert_eq!(via_value.index, 3);
    }

    #[test]
    fn embedding_deserializes_from_a_base64_string() {
        // 1.0f32 and -2.0f32, little-endian.
        let data = embed_data_from(r#""AACAPwAAAMA=""#).expect("base64");
        assert_eq!(data.embedding, vec![1.0, -2.0]);
        // The empty string is an empty vector.
        assert_eq!(embed_data_from(r#""""#).expect("empty").embedding.len(), 0);
        // Owned-string path (through a `Value`).
        let via_value: EmbedData = serde_json::from_value(
            serde_json::json!({"object":"embedding","index":0,"embedding":"AACAPwAAAMA="}),
        )
        .expect("from value");
        assert_eq!(via_value.embedding, vec![1.0, -2.0]);
    }

    #[test]
    fn embedding_rejects_malformed_base64_and_other_shapes() {
        // Three bytes: valid base64, not a whole number of f32s.
        let err = embed_data_from(r#""AAAA""#).expect_err("3 bytes");
        assert!(
            err.to_string()
                .contains("base64 embedding byte length is not a multiple of 4"),
            "{err}"
        );
        // Not base64 at all.
        assert!(embed_data_from(r#""not base64!""#).is_err());
        // Neither an array of numbers nor a string.
        for bad in [
            "null",
            "1.5",
            "true",
            r#"{"a":1}"#,
            r#"["a"]"#,
            "[[1.0]]",
            "[null]",
        ] {
            assert!(embed_data_from(bad).is_err(), "{bad} must be rejected");
        }
    }

    /// A two-vector body in the given per-item `embedding` JSON, with odd
    /// spacing and an unknown usage field a typed round trip would not keep.
    fn body_with(embeddings: [&str; 2], usage: &str) -> Bytes {
        Bytes::from(format!(
            "{{ \"object\":\"list\",\n \"data\":[{{\"object\":\"embedding\",\"index\":0,\
             \"embedding\":{}}}, {{\"embedding\":{},\"index\":1,\"object\":\"embedding\"}}],\
             \"model\":\"m\"{usage} }}",
            embeddings[0], embeddings[1]
        ))
    }

    const USAGE: &str = r#","usage":{"total_tokens":5,"prompt_tokens":5,"cost":0.1}"#;
    const FLOATS: &str = "[0.006929283495992422, -1e-3, 2]";
    const BASE64: &str = r#""AACAPwAAAMA=""#;

    #[test]
    fn a_body_in_the_wanted_encoding_is_kept_as_the_bytes_received() {
        for (vector, wanted) in [
            (FLOATS, EmbeddingEncoding::Float),
            (BASE64, EmbeddingEncoding::Base64),
        ] {
            let body = body_with([vector, vector], USAGE);
            let completion = EmbedCompletion::from_openai_body(body.clone(), wanted).unwrap();
            assert_eq!(completion.vector_count(), 2);
            let EmbedCompletion::Passthrough(upstream) = completion else {
                panic!("expected a passthrough for {wanted:?}");
            };
            assert_eq!(upstream.usage().prompt_tokens, 5);
            assert_eq!(upstream.into_body(), body);
        }
    }

    #[test]
    fn a_body_that_has_to_change_is_parsed() {
        let cases = [
            // Vectors in the other encoding have to be re-encoded.
            (
                body_with([FLOATS, FLOATS], USAGE),
                EmbeddingEncoding::Base64,
            ),
            (body_with([BASE64, BASE64], USAGE), EmbeddingEncoding::Float),
            // One vector in the other encoding is enough.
            (body_with([FLOATS, BASE64], USAGE), EmbeddingEncoding::Float),
            (
                body_with([FLOATS, BASE64], USAGE),
                EmbeddingEncoding::Base64,
            ),
            // No usage to trust: the flagged estimate has to be added.
            (body_with([FLOATS, FLOATS], ""), EmbeddingEncoding::Float),
            (
                body_with(
                    [FLOATS, FLOATS],
                    r#","usage":{"prompt_tokens":0,"total_tokens":0}"#,
                ),
                EmbeddingEncoding::Float,
            ),
        ];
        for (body, wanted) in cases {
            let completion = EmbedCompletion::from_openai_body(body.clone(), wanted).unwrap();
            assert!(
                matches!(completion, EmbedCompletion::Typed(_)),
                "{wanted:?}: {body:?}"
            );
            assert_eq!(completion.vector_count(), 2);
        }
    }

    #[test]
    fn a_usage_that_is_not_an_object_is_never_forwarded_verbatim() {
        let body = r#"{"object":"list","model":"m",
            "data":[{"object":"embedding","index":0,"embedding":[1.0]}],
            "usage":[5,5]}"#;
        let completion = EmbedCompletion::from_openai_body(
            Bytes::from_static(body.as_bytes()),
            EmbeddingEncoding::Float,
        );
        assert!(
            !matches!(completion, Ok(EmbedCompletion::Passthrough(_))),
            "{completion:?}"
        );
    }

    #[test]
    fn a_body_that_is_not_an_embeddings_response_is_an_error() {
        for body in [
            r#"{"usage":{"prompt_tokens":5}}"#,
            r#"{"data":"x","usage":{"prompt_tokens":5}}"#,
            // An item without `index`, or with a vector of the wrong type.
            r#"{"object":"list","model":"m","data":[{"object":"embedding","embedding":[1.0]}],"usage":{"prompt_tokens":5}}"#,
            r#"{"object":"list","model":"m","data":[{"object":"embedding","index":0,"embedding":7}],"usage":{"prompt_tokens":5}}"#,
            // Truncated after a complete item.
            r#"{"usage":{"prompt_tokens":5},"data":[{"index":0,"embedding":[1.0]},"#,
            // A top-level array is never an embeddings response, even when
            // its items line up with the fields the probe reads.
            r#"[[{"index":0,"embedding":[1.0]}],{"prompt_tokens":5}]"#,
            r#"[[],{"prompt_tokens":5}]"#,
            "[]",
            // Nor is a `data` item written as an array.
            r#"{"object":"list","model":"m","data":[[[1.0],0]],"usage":{"prompt_tokens":5}}"#,
            "not json",
        ] {
            assert!(
                EmbedCompletion::from_openai_body(
                    Bytes::from_static(body.as_bytes()),
                    EmbeddingEncoding::Float
                )
                .is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn output_encoding_is_base64_only_when_asked() {
        let mut req: EmbedRequest = serde_json::from_str(r#"{"model":"m","input":"x"}"#).unwrap();
        assert_eq!(req.output_encoding(), EmbeddingEncoding::Float);
        req.encoding_format = Some("float".to_owned());
        assert_eq!(req.output_encoding(), EmbeddingEncoding::Float);
        req.encoding_format = Some("base64".to_owned());
        assert_eq!(req.output_encoding(), EmbeddingEncoding::Base64);
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
