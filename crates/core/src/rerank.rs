//! Reranking types.
//!
//! The public request/response shape is Cohere-inspired but pinned by the M3
//! spec: a request carries a `query` and `documents` (each a bare string or a
//! `{ "text": ... }` object); the response carries `results` ordered by
//! descending `relevance_score`, each result's `index` pointing back to the
//! original document position, plus a `usage.search_units` count.
//!
//! Providers translate their own wire schema to/from these types; the gateway
//! (see `lumen_providers::rerank`) guarantees ordering, `top_n` clamping and
//! optional document echoing regardless of what the upstream does.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A document to rerank: a bare string, `{ "text": "..." }`, or (Cohere) an
/// arbitrary JSON object whose fields are selected by the request's
/// [`rank_fields`](RerankRequest::rank_fields).
///
/// Object documents are reduced to a single ranking text at the gateway edge
/// (see `lumen_providers::rerank`) before any provider is called, so providers
/// still only ever see plain text. When `rank_fields` is set, the named fields
/// are stringified and joined; otherwise the object's `text` field is used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RerankDocument {
    /// A bare string document.
    Text(String),
    /// An object document. `{ "text": "..." }` is the common case; any other
    /// fields are kept so `rank_fields` can select them.
    Object(Map<String, Value>),
}

impl RerankDocument {
    /// Borrow the document text, regardless of which form it took. An object
    /// without a string `text` field borrows the empty string (the ranking text
    /// is computed separately by [`into_rank_text`](Self::into_rank_text)).
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            RerankDocument::Text(s) => s,
            RerankDocument::Object(map) => {
                map.get("text").and_then(Value::as_str).unwrap_or_default()
            }
        }
    }

    /// Consume the document, yielding its `text` (the empty string for an object
    /// without a string `text` field).
    #[must_use]
    pub fn into_text(self) -> String {
        match self {
            RerankDocument::Text(s) => s,
            RerankDocument::Object(mut map) => match map.remove("text") {
                Some(Value::String(s)) => s,
                _ => String::new(),
            },
        }
    }

    /// Consume the document, reducing it to the single text used for ranking.
    ///
    /// Bare strings are returned unchanged (`rank_fields` never applies to
    /// them). For object documents: when `rank_fields` is `Some` and non-empty,
    /// the selected fields' values are stringified (strings verbatim, other JSON
    /// values via their compact JSON form) and joined with newlines in selector
    /// order, skipping absent fields (Cohere semantics); otherwise the object's
    /// `text` field is used.
    #[must_use]
    pub fn into_rank_text(self, rank_fields: Option<&[String]>) -> String {
        match self {
            RerankDocument::Text(s) => s,
            RerankDocument::Object(map) => match rank_fields {
                Some(fields) if !fields.is_empty() => fields
                    .iter()
                    .filter_map(|f| map.get(f).map(value_to_text))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => map
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_default(),
            },
        }
    }
}

/// Stringify a JSON value for ranking: strings verbatim, everything else via its
/// compact JSON representation.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A rerank request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankRequest {
    /// Client-facing model id.
    pub model: String,
    /// The query to score documents against.
    pub query: String,
    /// Documents to score. Accepts bare strings, `{ "text": ... }` objects, or
    /// arbitrary objects whose fields are selected by `rank_fields`.
    pub documents: Vec<RerankDocument>,
    /// Fields of object documents to concatenate for ranking, in order (Cohere's
    /// `rank_fields`). Ignored for bare-string documents. When omitted, object
    /// documents fall back to their `text` field. The gateway reduces each
    /// object document to a single ranking text at the edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_fields: Option<Vec<String>>,
    /// Return at most this many top results. Values larger than the document
    /// count are clamped silently by the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u32>,
    /// Echo each result's source document text back in `document`. Defaults to
    /// `false` to save bandwidth (M3 acceptance criterion 5).
    #[serde(default)]
    pub return_documents: bool,
}

/// The echoed document text, present in a result only when `return_documents`
/// was set on the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerankResultDocument {
    /// The source document's text.
    pub text: String,
}

/// A single scored result. `index` refers to the position in the request's
/// `documents` array (never the sorted position).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankResult {
    /// Position of this document in the original request.
    pub index: u32,
    /// Relevance score; higher is more relevant.
    pub relevance_score: f32,
    /// The echoed source document, present only when `return_documents` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<RerankResultDocument>,
}

/// Billing/accounting for a rerank call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RerankUsage {
    /// Number of search units billed (Cohere's unit; one per rerank call over a
    /// batch of up to 100 documents, upstream-defined).
    #[serde(default)]
    pub search_units: u32,
    /// `Some(true)` when the gateway derived the unit count itself because the
    /// upstream reported none (ADR 003); omitted for upstream-reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
}

/// A rerank response, ordered by descending `relevance_score`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankResponse {
    /// Scored results, most relevant first.
    pub results: Vec<RerankResult>,
    /// Usage accounting.
    #[serde(default)]
    pub usage: RerankUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_object_document_preserves_all_fields() {
        let doc: RerankDocument =
            serde_json::from_str(r#"{"title":"T","body":"B","score":3}"#).expect("valid object");
        assert!(matches!(doc, RerankDocument::Object(_)));
        // No `text` field: `text()` borrows the empty string.
        assert_eq!(doc.text(), "");
    }

    #[test]
    fn text_object_still_exposes_text() {
        let doc: RerankDocument =
            serde_json::from_str(r#"{"text":"hello","extra":1}"#).expect("valid object");
        assert_eq!(doc.text(), "hello");
        assert_eq!(doc.clone().into_text(), "hello");
        // Without rank_fields, ranking text is the `text` field.
        assert_eq!(doc.into_rank_text(None), "hello");
    }

    #[test]
    fn rank_fields_concatenate_selected_fields_in_order() {
        let doc: RerankDocument =
            serde_json::from_str(r#"{"title":"T","body":"B","meta":"M"}"#).expect("valid object");
        let fields = vec!["body".to_owned(), "title".to_owned()];
        // Selector order, not document order; absent fields skipped.
        assert_eq!(doc.into_rank_text(Some(&fields)), "B\nT");
    }

    #[test]
    fn rank_fields_stringify_non_string_values() {
        let doc: RerankDocument =
            serde_json::from_str(r#"{"n":42,"flag":true}"#).expect("valid object");
        let fields = vec!["n".to_owned(), "flag".to_owned()];
        assert_eq!(doc.into_rank_text(Some(&fields)), "42\ntrue");
    }

    #[test]
    fn rank_fields_do_not_apply_to_bare_strings() {
        let doc = RerankDocument::Text("bare".to_owned());
        let fields = vec!["title".to_owned()];
        assert_eq!(doc.into_rank_text(Some(&fields)), "bare");
    }

    #[test]
    fn request_parses_rank_fields() {
        let req: RerankRequest = serde_json::from_str(
            r#"{"model":"m","query":"q","documents":[{"title":"a"}],"rank_fields":["title"]}"#,
        )
        .expect("valid request");
        assert_eq!(
            req.rank_fields.as_deref(),
            Some(["title".to_owned()].as_slice())
        );
    }
}
