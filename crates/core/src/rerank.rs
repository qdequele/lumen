//! Reranking types.
//!
//! The public request/response shape is Cohere-inspired but pinned by the M3
//! spec: a request carries a `query` and `documents` (each a bare string or a
//! `{ "text": ... }` object); the response carries `results` ordered by
//! descending `relevance_score`, each result's `index` pointing back to the
//! original document position, plus a `usage.search_units` count.
//!
//! Providers translate their own wire schema to/from these types; the gateway
//! (see `ferrogate_providers::rerank`) guarantees ordering, `top_n` clamping and
//! optional document echoing regardless of what the upstream does.

use serde::{Deserialize, Serialize};

/// A document to rerank: either a bare string or `{ "text": "..." }`.
///
/// Both forms carry only text in v1 (Cohere also allows arbitrary objects with
/// a `rank_fields` selector — intentionally out of scope, see `docs/backlog.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RerankDocument {
    /// A bare string document.
    Text(String),
    /// An object document; only its `text` field is used.
    Object {
        /// The document text.
        text: String,
    },
}

impl RerankDocument {
    /// Borrow the document text, regardless of which form it took.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            RerankDocument::Text(s) | RerankDocument::Object { text: s } => s,
        }
    }

    /// Consume the document, yielding its text.
    #[must_use]
    pub fn into_text(self) -> String {
        match self {
            RerankDocument::Text(s) | RerankDocument::Object { text: s } => s,
        }
    }
}

/// A rerank request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerankRequest {
    /// Client-facing model id.
    pub model: String,
    /// The query to score documents against.
    pub query: String,
    /// Documents to score. Accepts bare strings or `{ "text": ... }` objects.
    pub documents: Vec<RerankDocument>,
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
