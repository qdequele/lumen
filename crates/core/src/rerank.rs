//! Reranking types, mirroring the Cohere `rerank` schema.

use serde::{Deserialize, Serialize};

/// A rerank request in Cohere format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerankRequest {
    pub model: String,
    pub query: String,
    /// Documents to score against the query. (Object documents are reduced to
    /// their text before v1; only strings are modelled here.)
    pub documents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_documents: Option<bool>,
}

/// The echoed document text, present only when `return_documents` was set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerankDocument {
    pub text: String,
}

/// A single scored result. `index` refers to the position in the request's
/// `documents` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankResult {
    pub index: u32,
    pub relevance_score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<RerankDocument>,
}

/// A rerank response in Cohere format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub results: Vec<RerankResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}
