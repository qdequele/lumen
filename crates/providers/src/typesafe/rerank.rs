//! Jev as a reranker: operator-authored templates from `/v1/rerank` to
//! SystemOne (ADR 013, 2026-09-26 amendment; ADR 014).
//!
//! A `typesafe` model that declares the `rerank` capability is served by
//! [`TypesafeRerankProvider`], driven by a [`RerankTemplate`]: an optional
//! static `context` (added to the SystemOne `state` next to the `query`) and
//! one of four strategies:
//!
//! - `noul`: one yes/no question per document; the noul (Jev's calibrated
//!   probability of "yes") is the `relevance_score`.
//! - `score`: one graded question per document; the score is normalised by
//!   the level count to `[0, 1]`.
//! - `composite`: several weighted yes/no questions per document; the score
//!   is their weighted mean.
//! - `choice`: a single listwise question over every document; the score is
//!   each option's probability. One upstream call, at most 255 documents.
//!
//! A `noul` question carries the document in structured instructions:
//!
//! ```json
//! "3": { "type": "noul",
//!        "instructions": { "document": "<document 3>", "question": "<template instructions>" },
//!        "criteria": { "true": "...", "false": "..." } }
//! ```
//!
//! Jev evaluates questions independently, so documents never see each other
//! (except under `choice`, which is listwise by design), and the query is
//! ingested once per call rather than once per document. Documents are packed
//! into as few calls as Jev's context allows (64k tokens per request, 32k for
//! the state plus the longest question) and the calls run concurrently.
//!
//! Templates are operator-authored (never client-supplied) and their text is
//! never logged: the `Debug` impls print only the strategy name.

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

/// Default question of the `score` strategy.
pub const DEFAULT_SCORE_INSTRUCTIONS: &str =
    "How well does `document` answer the query in the state?";
/// Default question of the `choice` strategy.
pub const DEFAULT_CHOICE_INSTRUCTIONS: &str =
    "Which option is the document that best answers the query in the state?";
/// Most questions a `composite` template may ask per document.
pub const MAX_COMPOSITE_QUESTIONS: usize = 8;
/// Most documents a `choice` rerank accepts (TypeSafe's option limit).
pub const MAX_CHOICE_DOCUMENTS: usize = lumen_core::systemone::MAX_CHOICE_OPTIONS;

/// One weighted yes/no criterion of a `composite` template.
#[derive(Clone, PartialEq)]
pub struct CompositeQuestion {
    /// The question asked about each `document`.
    pub instructions: String,
    /// What a yes means.
    pub criteria_true: String,
    /// What a no means.
    pub criteria_false: String,
    /// Weight in the mean, > 0.
    pub weight: f64,
}

impl std::fmt::Debug for CompositeQuestion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Template text is never logged: print the weight only.
        f.debug_struct("CompositeQuestion")
            .field("weight", &self.weight)
            .finish_non_exhaustive()
    }
}

/// How documents become SystemOne questions and answers become scores.
#[derive(Clone, PartialEq)]
pub enum RerankStrategy {
    /// One noul per document; the score is the noul.
    Noul {
        /// The yes/no question asked about each `document`.
        instructions: String,
        /// What a yes means.
        criteria_true: String,
        /// What a no means.
        criteria_false: String,
    },
    /// One graded score per document; the score is `score / (levels - 1)`.
    Score {
        /// The rating question asked about each `document`.
        instructions: String,
        /// Levels, worst first, 2 to 10.
        levels: Vec<String>,
    },
    /// Several nouls per document; the score is their weighted mean.
    Composite {
        /// The criteria, 1 to [`MAX_COMPOSITE_QUESTIONS`].
        questions: Vec<CompositeQuestion>,
    },
    /// One choice over all documents; the score is the option's probability.
    Choice {
        /// The listwise question.
        instructions: String,
    },
}

impl RerankStrategy {
    /// The strategy's name, safe to log.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Score { .. } => "score",
            Self::Composite { .. } => "composite",
            Self::Choice { .. } => "choice",
        }
    }
}

impl std::fmt::Debug for RerankStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Template text is never logged: print the strategy name only.
        f.debug_struct("RerankStrategy")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// An operator-authored SystemOne rerank template (ADR 014). Never
/// client-supplied, never logged.
#[derive(Clone, PartialEq)]
pub struct RerankTemplate {
    /// Static domain context added to the state as `context`.
    pub context: Option<String>,
    /// The question strategy.
    pub strategy: RerankStrategy,
}

impl std::fmt::Debug for RerankTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Template text is never logged: print the strategy name only.
        f.debug_struct("RerankTemplate")
            .field("strategy", &self.strategy.name())
            .field("has_context", &self.context.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for RerankTemplate {
    fn default() -> Self {
        Self {
            context: None,
            strategy: RerankStrategy::Noul {
                instructions: DEFAULT_INSTRUCTIONS.to_owned(),
                criteria_true: DEFAULT_CRITERIA_TRUE.to_owned(),
                criteria_false: DEFAULT_CRITERIA_FALSE.to_owned(),
            },
        }
    }
}

/// Serves `/v1/rerank` for a `typesafe` model through the SystemOne API.
pub struct TypesafeRerankProvider {
    inner: Arc<dyn SystemOneProvider>,
    provider_name: String,
    template: Arc<RerankTemplate>,
}

impl TypesafeRerankProvider {
    /// Wrap a SystemOne provider with a rerank template.
    #[must_use]
    pub fn new(
        inner: Arc<dyn SystemOneProvider>,
        provider_name: impl Into<String>,
        template: Arc<RerankTemplate>,
    ) -> Self {
        Self {
            inner,
            provider_name: provider_name.into(),
            template,
        }
    }
}

impl std::fmt::Debug for TypesafeRerankProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypesafeRerankProvider")
            .field("provider_name", &self.provider_name)
            .field("strategy", &self.template.strategy.name())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct State<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a str>,
}

#[derive(Serialize)]
struct NoulQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: Instructions<'a>,
    criteria: Criteria<'a>,
}

#[derive(Serialize)]
struct ScoreQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: Instructions<'a>,
    criteria: &'a [String],
}

#[derive(Serialize)]
struct ChoiceQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    /// Option id (`d<index>`) to document text, in document order.
    criteria: serde_json::Map<String, serde_json::Value>,
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

#[derive(Deserialize)]
struct ScoreAnswer {
    score: f32,
}

#[derive(Deserialize)]
struct ChoiceAnswer {
    probabilities: HashMap<String, f32>,
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
        let state = raw(&State {
            query: &req.query,
            context: self.template.context.as_deref(),
        })?;
        match &self.template.strategy {
            RerankStrategy::Choice { instructions } => {
                self.rerank_choice(&req, state, instructions, cancel).await
            }
            strategy => {
                self.rerank_per_document(&req, state, strategy, cancel)
                    .await
            }
        }
    }
}

/// The questions asked about document `i` (one for `noul` and `score`, one
/// per criterion for `composite`), keyed by question id.
fn document_questions(
    strategy: &RerankStrategy,
    i: usize,
    document: &str,
) -> Result<RawEntries, ProviderError> {
    match strategy {
        RerankStrategy::Noul {
            instructions,
            criteria_true,
            criteria_false,
        } => Ok(vec![(
            i.to_string(),
            raw(&NoulQuestion {
                kind: "noul",
                instructions: Instructions {
                    document,
                    question: instructions,
                },
                criteria: Criteria {
                    yes: criteria_true,
                    no: criteria_false,
                },
            })?,
        )]),
        RerankStrategy::Score {
            instructions,
            levels,
        } => Ok(vec![(
            i.to_string(),
            raw(&ScoreQuestion {
                kind: "score",
                instructions: Instructions {
                    document,
                    question: instructions,
                },
                criteria: levels,
            })?,
        )]),
        RerankStrategy::Composite { questions } => questions
            .iter()
            .enumerate()
            .map(|(c, q)| {
                Ok((
                    format!("{i}.{c}"),
                    raw(&NoulQuestion {
                        kind: "noul",
                        instructions: Instructions {
                            document,
                            question: &q.instructions,
                        },
                        criteria: Criteria {
                            yes: &q.criteria_true,
                            no: &q.criteria_false,
                        },
                    })?,
                ))
            })
            .collect(),
        RerankStrategy::Choice { .. } => Err(ProviderError::Translation(
            "typesafe rerank: choice is not per-document".to_owned(),
        )),
    }
}

/// Document `i`'s relevance score from the answers of its questions.
fn document_score(
    strategy: &RerankStrategy,
    answers: &HashMap<String, Box<RawValue>>,
    i: usize,
) -> Result<f32, ProviderError> {
    match strategy {
        RerankStrategy::Noul { .. } => Ok(answer::<NoulAnswer>(answers, &i.to_string())?.noul),
        RerankStrategy::Score { levels, .. } => {
            // At most `MAX_SCORE_LEVELS`, so the cast is exact.
            #[allow(clippy::cast_precision_loss)]
            let top = levels.len().saturating_sub(1).max(1) as f32;
            Ok(answer::<ScoreAnswer>(answers, &i.to_string())?.score / top)
        }
        RerankStrategy::Composite { questions } => {
            let mut sum = 0.0_f64;
            let mut weights = 0.0_f64;
            for (c, q) in questions.iter().enumerate() {
                let noul = answer::<NoulAnswer>(answers, &format!("{i}.{c}"))?.noul;
                sum += q.weight * f64::from(noul);
                weights += q.weight;
            }
            if weights <= 0.0 {
                return Err(ProviderError::Translation(
                    "typesafe rerank: composite weights must sum to more than zero".to_owned(),
                ));
            }
            // A weighted mean of probabilities in [0, 1].
            #[allow(clippy::cast_possible_truncation)]
            let score = (sum / weights) as f32;
            Ok(score)
        }
        RerankStrategy::Choice { .. } => Err(ProviderError::Translation(
            "typesafe rerank: choice is not per-document".to_owned(),
        )),
    }
}

impl TypesafeRerankProvider {
    /// `noul`, `score` and `composite`: questions per document, batched.
    async fn rerank_per_document(
        &self,
        req: &RerankRequest,
        state: Box<RawValue>,
        strategy: &RerankStrategy,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        // Per document: its question ids and bodies (composite asks several).
        let mut per_doc: Vec<RawEntries> = Vec::with_capacity(req.documents.len());
        let mut costs = Vec::with_capacity(req.documents.len());
        for (i, doc) in req.documents.iter().enumerate() {
            let questions = document_questions(strategy, i, truncate_doc(doc.text()))?;
            costs.push(
                questions
                    .iter()
                    .map(|(_, b)| tokens::estimate_text(b.get()))
                    .sum(),
            );
            per_doc.push(questions);
        }

        let calls = plan_calls(tokens::estimate_text(state.get()), &costs);
        let mut remaining = per_doc.into_iter();
        let batches: Vec<SystemOneRequest> = calls
            .iter()
            .map(|range| {
                let batch: RawEntries = remaining.by_ref().take(range.len()).flatten().collect();
                SystemOneRequest::new(req.model.clone(), state.clone(), batch)
            })
            .collect();

        let responses: Vec<_> = stream::iter(batches)
            .map(|batch| self.inner.evaluate(batch, cancel.clone()))
            .buffered(CALL_CONCURRENCY)
            .try_collect()
            .await?;

        let mut results = Vec::with_capacity(req.documents.len());
        let mut usage = UsageSum::default();
        for (range, response) in calls.iter().zip(&responses) {
            usage.add(response);
            let answers = parse_answers(response)?;
            for i in range.clone() {
                results.push(RerankResult {
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                    relevance_score: document_score(strategy, &answers, i)?,
                    document: None,
                });
            }
        }
        Ok(usage.into_response(results))
    }

    /// `choice`: one question over every document, one call.
    async fn rerank_choice(
        &self,
        req: &RerankRequest,
        state: Box<RawValue>,
        instructions: &str,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        if req.documents.len() > MAX_CHOICE_DOCUMENTS {
            return Err(ProviderError::UnsupportedInput {
                provider: self.provider_name.clone(),
                reason: format!("a choice rerank of more than {MAX_CHOICE_DOCUMENTS} documents"),
            });
        }
        if req.documents.is_empty() {
            return Ok(UsageSum::default().into_response(Vec::new()));
        }
        let mut options = serde_json::Map::with_capacity(req.documents.len());
        for (i, doc) in req.documents.iter().enumerate() {
            options.insert(
                format!("d{i}"),
                serde_json::Value::String(truncate_doc(doc.text()).to_owned()),
            );
        }
        let question = raw(&ChoiceQuestion {
            kind: "choice",
            instructions,
            criteria: options,
        })?;
        if tokens::estimate_text(state.get()) + tokens::estimate_text(question.get())
            > MAX_CALL_TOKENS
        {
            return Err(ProviderError::UnsupportedInput {
                provider: self.provider_name.clone(),
                reason: "a choice rerank whose documents do not fit one call".to_owned(),
            });
        }
        let batch: RawEntries = vec![("rank".to_owned(), question)];
        let response = self
            .inner
            .evaluate(
                SystemOneRequest::new(req.model.clone(), state, batch),
                cancel,
            )
            .await?;
        let mut usage = UsageSum::default();
        usage.add(&response);
        let answers = parse_answers(&response)?;
        let choice = answer::<ChoiceAnswer>(&answers, "rank")?;
        let results = (0..req.documents.len())
            .map(|i| RerankResult {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                relevance_score: choice
                    .probabilities
                    .get(&format!("d{i}"))
                    .copied()
                    .unwrap_or(0.0),
                document: None,
            })
            .collect();
        Ok(usage.into_response(results))
    }
}

/// Upstream input tokens summed over the calls; any call without usage makes
/// the total unknown (the handler then estimates, ADR 003).
#[derive(Default)]
struct UsageSum {
    total: u32,
    incomplete: bool,
}

impl UsageSum {
    fn add(&mut self, response: &lumen_core::SystemOneResponse) {
        match response.usage {
            Some(u) if u.input_tokens > 0 => self.total = self.total.saturating_add(u.input_tokens),
            _ => self.incomplete = true,
        }
    }

    fn into_response(self, results: Vec<RerankResult>) -> RerankResponse {
        RerankResponse {
            results,
            usage: RerankUsage {
                // Jev bills tokens, not search units: the gateway derives
                // units (ADR 003). A call without usage makes the whole
                // count an estimate rather than a partial upstream sum.
                total_tokens: if self.incomplete { 0 } else { self.total },
                ..RerankUsage::default()
            },
        }
    }
}

fn parse_answers(
    response: &lumen_core::SystemOneResponse,
) -> Result<HashMap<String, Box<RawValue>>, ProviderError> {
    response
        .answers()
        .map(|a| serde_json::from_str(a.get()))
        .transpose()
        .map_err(|e| ProviderError::Translation(format!("typesafe rerank answers: {e}")))
        .map(Option::unwrap_or_default)
}

fn answer<T: serde::de::DeserializeOwned>(
    answers: &HashMap<String, Box<RawValue>>,
    id: &str,
) -> Result<T, ProviderError> {
    let raw = answers.get(id).ok_or_else(|| {
        ProviderError::Translation(format!("typesafe rerank: no answer for `{id}`"))
    })?;
    serde_json::from_str(raw.get())
        .map_err(|e| ProviderError::Translation(format!("typesafe rerank answer `{id}`: {e}")))
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

    use async_trait::async_trait;
    use lumen_core::{RerankDocument, SystemOneResponse};
    use std::sync::Mutex;

    /// A fake SystemOne upstream: records each request's state and questions
    /// JSON and answers every question with `answer(question_id)`.
    struct Fake {
        seen: Mutex<Vec<String>>,
        answer: fn(&str) -> serde_json::Value,
    }

    #[async_trait]
    impl SystemOneProvider for Fake {
        async fn evaluate(
            &self,
            req: SystemOneRequest,
            _cancel: CancellationToken,
        ) -> Result<SystemOneResponse, ProviderError> {
            let mut answers = serde_json::Map::new();
            let mut questions = Vec::new();
            for (id, body) in req.questions() {
                questions.push(format!("{id}={}", body.get()));
                answers.insert(id.clone(), (self.answer)(id));
            }
            self.seen.lock().unwrap().push(format!(
                "state={} {}",
                req.state().get(),
                questions.join(" ")
            ));
            let body = serde_json::json!({
                "model": "jev-1", "answers": answers,
                "usage": { "input_tokens": 10, "output_tokens": 1 }
            });
            Ok(serde_json::from_value(body).unwrap())
        }
    }

    fn request(docs: &[&str]) -> RerankRequest {
        RerankRequest {
            model: "jev-latest".into(),
            query: "q".into(),
            documents: docs
                .iter()
                .map(|d| RerankDocument::Text((*d).to_owned()))
                .collect(),
            rank_fields: None,
            top_n: None,
            return_documents: false,
        }
    }

    fn provider(
        template: RerankTemplate,
        answer: fn(&str) -> serde_json::Value,
    ) -> (TypesafeRerankProvider, Arc<Fake>) {
        let fake = Arc::new(Fake {
            seen: Mutex::new(Vec::new()),
            answer,
        });
        let p = TypesafeRerankProvider::new(fake.clone(), "typesafe", Arc::new(template));
        (p, fake)
    }

    #[tokio::test]
    async fn noul_scores_are_the_noul_and_context_reaches_the_state() {
        let t = RerankTemplate {
            context: Some("legal".into()),
            ..RerankTemplate::default()
        };
        let (p, fake) = provider(
            t,
            |id| serde_json::json!({ "type": "noul", "noul": if id == "0" { 0.9 } else { 0.1 } }),
        );
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        let scores: Vec<f32> = out.results.iter().map(|r| r.relevance_score).collect();
        assert_eq!(scores, vec![0.9, 0.1]);
        let seen = fake.seen.lock().unwrap().join("\n");
        assert!(seen.contains(r#""context":"legal""#), "{seen}");
        assert!(seen.contains(r#""type":"noul""#));
    }

    #[tokio::test]
    async fn no_context_leaves_the_state_as_the_bare_query() {
        let (p, fake) = provider(
            RerankTemplate::default(),
            |_| serde_json::json!({ "type": "noul", "noul": 0.5 }),
        );
        p.rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap().join("\n");
        assert!(seen.starts_with(r#"state={"query":"q"} "#), "{seen}");
    }

    #[tokio::test]
    async fn score_is_normalised_by_the_level_count() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Score {
                instructions: "rate".into(),
                levels: vec!["off".into(), "partly".into(), "fully".into()],
            },
        };
        let (p, fake) = provider(t, |_| serde_json::json!({ "type": "score", "score": 1.5 }));
        let out = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap();
        assert!((out.results[0].relevance_score - 0.75).abs() < 1e-6);
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert!(seen.contains(r#""type":"score""#), "{seen}");
        assert!(
            seen.contains(r#""criteria":["off","partly","fully"]"#),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn composite_is_the_weighted_mean_of_its_nouls() {
        let q = |w: f64| CompositeQuestion {
            instructions: "i".into(),
            criteria_true: "t".into(),
            criteria_false: "f".into(),
            weight: w,
        };
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Composite {
                questions: vec![q(1.0), q(3.0)],
            },
        };
        // question ids are "<doc>.<criterion>": criterion 0 answers 1.0, criterion 1 answers 0.0.
        let (p, fake) = provider(t, |id| {
            let v = if id.ends_with(".0") { 1.0 } else { 0.0 };
            serde_json::json!({ "type": "noul", "noul": v })
        });
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        let scores: Vec<f32> = out.results.iter().map(|r| r.relevance_score).collect();
        assert_eq!(scores.len(), 2);
        assert!(scores.iter().all(|s| (s - 0.25).abs() < 1e-6), "{scores:?}");
        // Four questions (two documents x two criteria) share one call.
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        for id in ["0.0=", "0.1=", "1.0=", "1.1="] {
            assert!(seen[0].contains(id), "{id} in {}", seen[0]);
        }
    }

    #[tokio::test]
    async fn composite_with_zero_total_weight_is_an_error_not_nan() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Composite {
                questions: vec![CompositeQuestion {
                    instructions: "i".into(),
                    criteria_true: "t".into(),
                    criteria_false: "f".into(),
                    weight: 0.0,
                }],
            },
        };
        let (p, _) = provider(t, |_| serde_json::json!({ "type": "noul", "noul": 1.0 }));
        let err = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
    }

    #[tokio::test]
    async fn choice_uses_one_question_and_option_probabilities() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Choice {
                instructions: "best?".into(),
            },
        };
        let (p, fake) = provider(t, |_| {
            serde_json::json!({
                "type": "choice", "choice": "d1",
                "probabilities": { "d0": 0.2, "d1": 0.8 }, "confidence": 0.7
            })
        });
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        let scores: Vec<f32> = out.results.iter().map(|r| r.relevance_score).collect();
        assert_eq!(scores, vec![0.2, 0.8]);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].contains(r#""d0":"a""#) && seen[0].contains(r#""d1":"b""#),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn choice_over_255_documents_is_rejected_before_any_call() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Choice {
                instructions: "best?".into(),
            },
        };
        let (p, fake) = provider(t, |_| serde_json::json!({}));
        let docs: Vec<String> = (0..256).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let err = p
            .rerank(request(&refs), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::UnsupportedInput { .. }),
            "{err:?}"
        );
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn debug_output_never_carries_template_text() {
        let t = RerankTemplate {
            context: Some("SECRET-CONTEXT".into()),
            strategy: RerankStrategy::Composite {
                questions: vec![CompositeQuestion {
                    instructions: "SECRET-QUESTION".into(),
                    criteria_true: "SECRET-TRUE".into(),
                    criteria_false: "SECRET-FALSE".into(),
                    weight: 1.0,
                }],
            },
        };
        let (p, _) = provider(t.clone(), |_| serde_json::json!({}));
        let debug = format!("{t:?} {:?} {p:?}", t.strategy);
        assert!(!debug.contains("SECRET"), "{debug}");
        assert!(debug.contains("composite"), "{debug}");
    }
}
