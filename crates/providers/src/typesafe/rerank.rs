//! Jev as a reranker: a converter from `/v1/rerank` to SystemOne (ADR 013,
//! 2026-09-26 amendment).
//!
//! A `typesafe` model that declares the `rerank` capability is served by
//! [`TypesafeRerankProvider`]. Each rerank request becomes SystemOne calls
//! whose `state` is `{"query": ...}` and whose questions are one `noul` per
//! document, the document carried in structured instructions:
//!
//! ```json
//! "3": { "type": "noul",
//!        "instructions": { "document": "<document 3>", "question": "<converter instructions>" },
//!        "criteria": { "true": "...", "false": "..." } }
//! ```
//!
//! The noul (Jev's calibrated probability of "yes") is the document's
//! `relevance_score`. Jev evaluates questions independently, so documents
//! never see each other, and the query is ingested once per call rather than
//! once per document. Documents are packed into as few calls as Jev's
//! context allows (64k tokens per request, 32k for the state plus the
//! longest question) and the calls run concurrently.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, StreamExt, TryStreamExt};
use lumen_core::systemone::RawEntries;
use lumen_core::{
    tokens, ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult,
    RerankUsage, SystemOneProvider, SystemOneRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::value::{to_raw_value, RawValue};
use tokio_util::sync::CancellationToken;

/// Default question asked about every document.
pub const DEFAULT_INSTRUCTIONS: &str =
    "Is `document` relevant to the query in the state, that is, does it answer the query \
     or contain information that directly helps answer it?";
/// Default meaning of a yes.
pub const DEFAULT_CRITERIA_TRUE: &str =
    "The document answers the query or contains information that directly helps answer it.";
/// Default meaning of a no.
pub const DEFAULT_CRITERIA_FALSE: &str =
    "The document is off-topic, or only shares keywords with the query without helping answer it.";

/// Documents longer than this (estimated tokens) are truncated before being
/// sent, so any single document always fits Jev's 32k state-plus-question
/// budget. Mirrors Cohere's default `max_tokens_per_doc`.
pub const MAX_DOC_TOKENS: u64 = 4_096;
/// Estimated-token budget of one upstream call, a safety margin under Jev's
/// 64k per-request limit (the estimate is a byte heuristic).
const MAX_CALL_TOKENS: u64 = 48_000;
/// Most documents per upstream call.
const MAX_DOCS_PER_CALL: usize = 100;
/// Upstream calls in flight at once for one rerank request.
const CALL_CONCURRENCY: usize = 4;

/// How a rerank request is turned into SystemOne questions. Operator-authored
/// (config), never client-supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankConverter {
    /// The yes/no question asked about each `document`, relative to the
    /// query in the state.
    pub instructions: String,
    /// What a yes means.
    pub criteria_true: String,
    /// What a no means.
    pub criteria_false: String,
}

impl Default for RerankConverter {
    fn default() -> Self {
        Self {
            instructions: DEFAULT_INSTRUCTIONS.to_owned(),
            criteria_true: DEFAULT_CRITERIA_TRUE.to_owned(),
            criteria_false: DEFAULT_CRITERIA_FALSE.to_owned(),
        }
    }
}

/// Serves `/v1/rerank` for a `typesafe` model through the SystemOne API.
pub struct TypesafeRerankProvider {
    inner: Arc<dyn SystemOneProvider>,
    provider_name: String,
    converter: RerankConverter,
}

impl TypesafeRerankProvider {
    /// Wrap a SystemOne provider with a converter.
    #[must_use]
    pub fn new(
        inner: Arc<dyn SystemOneProvider>,
        provider_name: impl Into<String>,
        converter: RerankConverter,
    ) -> Self {
        Self {
            inner,
            provider_name: provider_name.into(),
            converter,
        }
    }
}

impl std::fmt::Debug for TypesafeRerankProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypesafeRerankProvider")
            .field("provider_name", &self.provider_name)
            .field("converter", &self.converter)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct State<'a> {
    query: &'a str,
}

#[derive(Serialize)]
struct Question<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: Instructions<'a>,
    criteria: Criteria<'a>,
}

#[derive(Serialize)]
struct Instructions<'a> {
    document: &'a str,
    question: &'a str,
}

#[derive(Serialize)]
struct Criteria<'a> {
    #[serde(rename = "true")]
    yes: &'a str,
    #[serde(rename = "false")]
    no: &'a str,
}

#[derive(Deserialize)]
struct NoulAnswer {
    noul: f32,
}

/// Truncate `text` to about `MAX_DOC_TOKENS` estimated tokens, on a char
/// boundary.
fn truncate_doc(text: &str) -> &str {
    let max_bytes = usize::try_from(MAX_DOC_TOKENS * 4).unwrap_or(usize::MAX);
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Split documents (by their estimated per-question cost) into contiguous
/// index ranges that each fit one upstream call.
fn plan_calls(query_tokens: u64, question_tokens: &[u64]) -> Vec<std::ops::Range<usize>> {
    let mut calls = Vec::new();
    let mut start = 0;
    let mut used = query_tokens;
    for (i, &cost) in question_tokens.iter().enumerate() {
        let full = i - start >= MAX_DOCS_PER_CALL || (i > start && used + cost > MAX_CALL_TOKENS);
        if full {
            calls.push(start..i);
            start = i;
            used = query_tokens;
        }
        used += cost;
    }
    if start < question_tokens.len() {
        calls.push(start..question_tokens.len());
    }
    calls
}

fn raw<T: Serialize>(value: &T) -> Result<Box<RawValue>, ProviderError> {
    to_raw_value(value).map_err(|e| ProviderError::Translation(format!("typesafe rerank: {e}")))
}

#[async_trait]
impl RerankProvider for TypesafeRerankProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let state = raw(&State { query: &req.query })?;
        let criteria = Criteria {
            yes: &self.converter.criteria_true,
            no: &self.converter.criteria_false,
        };

        let mut questions: Vec<(String, Box<RawValue>)> = Vec::with_capacity(req.documents.len());
        let mut costs = Vec::with_capacity(req.documents.len());
        for (i, doc) in req.documents.iter().enumerate() {
            let body = raw(&Question {
                kind: "noul",
                instructions: Instructions {
                    document: truncate_doc(doc.text()),
                    question: &self.converter.instructions,
                },
                criteria: Criteria { ..criteria },
            })?;
            costs.push(tokens::estimate_text(body.get()));
            questions.push((i.to_string(), body));
        }

        let calls = plan_calls(tokens::estimate_text(state.get()), &costs);
        let mut remaining = questions.into_iter();
        let batches: Vec<SystemOneRequest> = calls
            .iter()
            .map(|range| {
                let batch: RawEntries = remaining.by_ref().take(range.len()).collect();
                SystemOneRequest::new(req.model.clone(), state.clone(), batch)
            })
            .collect();

        let responses: Vec<_> = stream::iter(batches)
            .map(|batch| self.inner.evaluate(batch, cancel.clone()))
            .buffered(CALL_CONCURRENCY)
            .try_collect()
            .await?;

        let mut results = Vec::with_capacity(req.documents.len());
        let mut total_tokens: u32 = 0;
        let mut usage_complete = true;
        for (range, response) in calls.iter().zip(&responses) {
            let answers: HashMap<String, NoulAnswer> = response
                .answers()
                .map(|a| serde_json::from_str(a.get()))
                .transpose()
                .map_err(|e| ProviderError::Translation(format!("typesafe rerank answers: {e}")))?
                .unwrap_or_default();
            for i in range.clone() {
                let answer = answers.get(&i.to_string()).ok_or_else(|| {
                    ProviderError::Translation(format!(
                        "typesafe rerank: no answer for document {i}"
                    ))
                })?;
                results.push(RerankResult {
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                    relevance_score: answer.noul,
                    document: None,
                });
            }
            match response.usage {
                Some(usage) if usage.input_tokens > 0 => {
                    total_tokens = total_tokens.saturating_add(usage.input_tokens);
                }
                _ => usage_complete = false,
            }
        }

        Ok(RerankResponse {
            results,
            usage: RerankUsage {
                // Jev bills tokens, not search units: the gateway derives
                // units (ADR 003). A call without usage makes the whole
                // count an estimate rather than a partial upstream sum.
                total_tokens: if usage_complete { total_tokens } else { 0 },
                ..RerankUsage::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_split_on_document_count_and_token_budget() {
        // 250 cheap documents: split every 100.
        let cheap = vec![10; 250];
        assert_eq!(plan_calls(5, &cheap), vec![0..100, 100..200, 200..250]);
        // Big documents: split when the next one would overflow the budget.
        let big = vec![20_000; 5];
        assert_eq!(plan_calls(5, &big), vec![0..2, 2..4, 4..5]);
        // A single document always gets its own call, even if oversized.
        assert_eq!(plan_calls(5, &[60_000]), vec![0..1]);
        assert!(plan_calls(5, &[]).is_empty());
    }

    #[test]
    fn long_documents_are_truncated_on_a_char_boundary() {
        let text = "é".repeat(20_000); // 40_000 bytes
        let cut = truncate_doc(&text);
        assert!(cut.len() <= 16_384);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(truncate_doc("short"), "short");
    }
}
