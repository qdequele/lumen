//! Any decision model as a reranker (ADR 014, ADR 016): operator-authored
//! templates from `/v1/rerank` to a [`DecisionProvider`].
//!
//! A rerank virtual model's target that carries a `remap` onto a decision
//! model (TypeSafe, Perplexity, OpenAI, Ollama, Cloudflare) is served by
//! [`DecisionRerankProvider`], driven by the [`RerankTemplate`] compiled from
//! that remap: an optional static `context` (added to the decision input
//! next to the `query`) and one of four strategies:
//!
//! - `predicate` (formerly `noul`): one yes/no question per document; the
//!   model's calibrated probability of "yes" is the `relevance_score`.
//! - `score`: one graded question per document; the score is normalised by
//!   the level count to `[0, 1]`.
//! - `composite`: several weighted yes/no questions per document; the score
//!   is their weighted mean.
//! - `choice`: a single listwise question over every document; the score is
//!   each option's probability. One upstream call, at most the target's
//!   option limit (255 on TypeSafe, 26 on Ollama).
//!
//! A predicate question carries the document in structured instructions,
//! `{"document": "<document 3>", "question": "<template instructions>"}`, so
//! a TypeSafe-family target receives exactly the bytes Jev always received
//! (D7) and OpenAI reads the same object as compact JSON text.
//!
//! Questions are evaluated independently, so documents never see each other
//! (except under `choice`, which is listwise by design), and the query is
//! ingested once per call rather than once per document. Documents are packed
//! into as few calls as the target's [`PackLimits`](lumen_core::decisions::PackLimits)
//! allow (estimated tokens and documents per call, capped by its question
//! limit), with the target's concurrency.
//!
//! A refused `predicate` or `score` question scores its document (or its
//! composite criterion) 0.0 and is counted in [`RerankUsage::refusals`]; a
//! refused `choice` fails the attempt as `content_filter`.
//!
//! Templates are operator-authored (never client-supplied) and their text is
//! never logged: the `Debug` impls print only the strategy name.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, StreamExt, TryStreamExt};
use lumen_core::decisions::{
    Answer, ChoiceOption, ChoiceValue, DecisionLimits, DecisionRequest, DecisionResponse, Input,
    Level, PredicateCriteria, Question, QuestionKind, Text,
};
use lumen_core::{
    tokens, DecisionProvider, ProviderError, RerankProvider, RerankRequest, RerankResponse,
    RerankResult, RerankUsage,
};
use serde::Serialize;

use super::family::question_body_len;
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
/// sent, so any single document always fits a target's call budget. Mirrors
/// Cohere's default `max_tokens_per_doc`.
pub const MAX_DOC_TOKENS: u64 = 4_096;

/// Default question of the `score` strategy.
pub const DEFAULT_SCORE_INSTRUCTIONS: &str =
    "How well does `document` answer the query in the state?";
/// Default question of the `choice` strategy.
pub const DEFAULT_CHOICE_INSTRUCTIONS: &str =
    "Which option is the document that best answers the query in the state?";
/// Most questions a `composite` template may ask per document.
pub const MAX_COMPOSITE_QUESTIONS: usize = 8;

/// Bytes per estimated token, the heuristic of [`tokens::estimate_text`].
const BYTES_PER_TOKEN: u64 = 4;

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

/// How documents become decision questions and answers become scores.
#[derive(Clone, PartialEq)]
pub enum RerankStrategy {
    /// One predicate per document; the score is its probability of yes.
    Predicate {
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
    /// Several predicates per document; the score is their weighted mean.
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
            Self::Predicate { .. } => "predicate",
            Self::Score { .. } => "score",
            Self::Composite { .. } => "composite",
            Self::Choice { .. } => "choice",
        }
    }

    /// Questions asked per document (1, or one per composite criterion).
    fn questions_per_doc(&self) -> usize {
        match self {
            Self::Composite { questions } => questions.len().max(1),
            _ => 1,
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

/// An operator-authored rerank template (ADR 014). Never client-supplied,
/// never logged.
#[derive(Clone, PartialEq)]
pub struct RerankTemplate {
    /// Static domain context added to the input as `context`.
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
            strategy: RerankStrategy::Predicate {
                instructions: DEFAULT_INSTRUCTIONS.to_owned(),
                criteria_true: DEFAULT_CRITERIA_TRUE.to_owned(),
                criteria_false: DEFAULT_CRITERIA_FALSE.to_owned(),
            },
        }
    }
}

/// Called with the number of questions refused in an attempt that fails
/// because of them (a refused listwise `choice`), which never reaches
/// [`RerankUsage::refusals`]. Feeds `lumen_decision_refusals_total`.
pub type RefusalHook = Arc<dyn Fn(u64) + Send + Sync>;

/// Serves `/v1/rerank` through any decision model.
pub struct DecisionRerankProvider {
    inner: Arc<dyn DecisionProvider>,
    provider_name: String,
    template: Arc<RerankTemplate>,
    on_refusal: Option<RefusalHook>,
}

impl DecisionRerankProvider {
    /// Wrap a decision provider with a rerank template.
    #[must_use]
    pub fn new(
        inner: Arc<dyn DecisionProvider>,
        provider_name: impl Into<String>,
        template: Arc<RerankTemplate>,
    ) -> Self {
        Self {
            inner,
            provider_name: provider_name.into(),
            template,
            on_refusal: None,
        }
    }

    /// Report refusals that fail an attempt (a refused `choice`) to `hook`;
    /// refusals in a served response travel in [`RerankUsage::refusals`].
    #[must_use]
    pub fn with_refusal_hook(mut self, hook: RefusalHook) -> Self {
        self.on_refusal = Some(hook);
        self
    }
}

impl std::fmt::Debug for DecisionRerankProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionRerankProvider")
            .field("provider_name", &self.provider_name)
            .field("strategy", &self.template.strategy.name())
            .field("refusal_hook", &self.on_refusal.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct State<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a str>,
}

/// The structured instructions of a per-document question. Field order is
/// wire order: `document` before `question` (D7).
#[derive(Serialize)]
struct Instructions<'a> {
    document: &'a str,
    question: &'a str,
}

fn translation(msg: impl std::fmt::Display) -> ProviderError {
    ProviderError::Translation(format!("decisions rerank: {msg}"))
}

fn raw<T: Serialize>(value: &T) -> Result<Box<RawValue>, ProviderError> {
    to_raw_value(value).map_err(translation)
}

fn instructions(document: &str, question: &str) -> Result<Text, ProviderError> {
    raw(&Instructions { document, question }).map(Text::Json)
}

fn predicate(
    id: String,
    document: &str,
    question: &str,
    yes: &str,
    no: &str,
) -> Result<Question, ProviderError> {
    Ok(Question {
        name: Some(id),
        instructions: Some(instructions(document, question)?),
        kind: QuestionKind::Predicate {
            criteria: Some(PredicateCriteria {
                when_true: Some(Text::Plain(yes.to_owned())),
                when_false: Some(Text::Plain(no.to_owned())),
            }),
        },
        raw: None,
    })
}

/// Truncate `text` to about `MAX_DOC_TOKENS` estimated tokens, on a char
/// boundary.
fn truncate_doc(text: &str) -> &str {
    let max_bytes = usize::try_from(MAX_DOC_TOKENS * BYTES_PER_TOKEN).unwrap_or(usize::MAX);
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Estimated tokens of `bytes` wire bytes.
fn bytes_to_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .div_ceil(BYTES_PER_TOKEN)
}

/// Estimated tokens of a question: [`tokens::estimate_text`] of its
/// TypeSafe wire body without the id, per question, exactly as Jev's rerank
/// always packed (D7). Every target packs by this one measure.
fn question_tokens(q: &Question) -> Result<u64, ProviderError> {
    question_body_len(q).map(bytes_to_tokens)
}

/// Documents per call: the profile's cap, lowered so a call never exceeds
/// the target's question limit (composite asks several per document).
fn docs_per_call(limits: &DecisionLimits, questions_per_doc: usize) -> usize {
    let by_questions = limits
        .max_questions
        .map_or(usize::MAX, |q| (q / questions_per_doc.max(1)).max(1));
    limits.pack.max_docs_per_call.max(1).min(by_questions)
}

/// Split documents (by their estimated per-document cost) into contiguous
/// index ranges that each fit one upstream call: at most `max_docs`
/// documents and `max_call_tokens` estimated tokens (a single oversized
/// document still gets its own call).
fn plan_calls(
    query_tokens: u64,
    costs: &[u64],
    max_call_tokens: u64,
    max_docs: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut calls = Vec::new();
    let mut start = 0;
    let mut used = query_tokens;
    for (i, &cost) in costs.iter().enumerate() {
        let full = i - start >= max_docs || (i > start && used + cost > max_call_tokens);
        if full {
            calls.push(start..i);
            start = i;
            used = query_tokens;
        }
        used += cost;
    }
    if start < costs.len() {
        calls.push(start..costs.len());
    }
    calls
}

/// The questions asked about document `i` (one for `predicate` and `score`,
/// one per criterion for `composite`), named `"{i}"` or `"{i}.{c}"`.
fn document_questions(
    strategy: &RerankStrategy,
    i: usize,
    document: &str,
) -> Result<Vec<Question>, ProviderError> {
    match strategy {
        RerankStrategy::Predicate {
            instructions: question,
            criteria_true,
            criteria_false,
        } => Ok(vec![predicate(
            i.to_string(),
            document,
            question,
            criteria_true,
            criteria_false,
        )?]),
        RerankStrategy::Score {
            instructions: question,
            levels,
        } => Ok(vec![Question {
            name: Some(i.to_string()),
            instructions: Some(instructions(document, question)?),
            kind: QuestionKind::Score {
                levels: levels
                    .iter()
                    .map(|level| Level {
                        label: Text::Plain(level.clone()),
                        description: None,
                    })
                    .collect(),
            },
            raw: None,
        }]),
        RerankStrategy::Composite { questions } => questions
            .iter()
            .enumerate()
            .map(|(c, q)| {
                predicate(
                    format!("{i}.{c}"),
                    document,
                    &q.instructions,
                    &q.criteria_true,
                    &q.criteria_false,
                )
            })
            .collect(),
        RerankStrategy::Choice { .. } => Err(translation("choice is not per-document")),
    }
}

/// An upstream probability or normalised score as a `relevance_score`:
/// clamped to `[0, 1]` (the module contract), and rejected when not finite so
/// a NaN can never reach the handler's sort.
fn unit_score(v: f64) -> Result<f32, ProviderError> {
    if v.is_finite() {
        // Clamped to [0, 1] first, so the narrowing loses precision only.
        #[allow(clippy::cast_possible_truncation)]
        Ok(v.clamp(0.0, 1.0) as f32)
    } else {
        Err(translation("a non-finite score in the answer"))
    }
}

/// The probability of a predicate answer; a refusal is counted and scores 0.
fn probability(answer: &Answer, refusals: &mut u32) -> Result<f64, ProviderError> {
    match answer {
        Answer::Predicate { probability } => Ok(*probability),
        Answer::Refusal => {
            *refusals += 1;
            Ok(0.0)
        }
        _ => Err(translation("expected a predicate answer")),
    }
}

/// A document's relevance score from the answers of its questions, in
/// question order (one for `predicate` and `score`, one per criterion for
/// `composite`).
fn document_score(
    strategy: &RerankStrategy,
    answers: &[Answer],
    refusals: &mut u32,
) -> Result<f32, ProviderError> {
    let first = || {
        answers
            .first()
            .ok_or_else(|| translation("no answer for a document"))
    };
    match strategy {
        RerankStrategy::Predicate { .. } => unit_score(probability(first()?, refusals)?),
        RerankStrategy::Score { levels, .. } => match first()? {
            Answer::Score { score, .. } => {
                // At most `MAX_SCORE_LEVELS`, so the cast is exact.
                #[allow(clippy::cast_precision_loss)]
                let top = levels.len().saturating_sub(1).max(1) as f64;
                unit_score(score / top)
            }
            Answer::Refusal => {
                *refusals += 1;
                Ok(0.0)
            }
            _ => Err(translation("expected a score answer")),
        },
        RerankStrategy::Composite { questions } => {
            if answers.len() != questions.len() {
                return Err(translation("a composite document is missing answers"));
            }
            let mut sum = 0.0_f64;
            let mut weights = 0.0_f64;
            for (q, a) in questions.iter().zip(answers) {
                let p = unit_score(probability(a, refusals)?)?;
                sum += q.weight * f64::from(p);
                weights += q.weight;
            }
            if weights <= 0.0 {
                return Err(translation("composite weights must sum to more than zero"));
            }
            // A weighted mean of probabilities in [0, 1].
            #[allow(clippy::cast_possible_truncation)]
            let score = (sum / weights) as f32;
            Ok(score)
        }
        RerankStrategy::Choice { .. } => Err(translation("choice is not per-document")),
    }
}

#[async_trait]
impl RerankProvider for DecisionRerankProvider {
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

impl DecisionRerankProvider {
    /// Fail before any call when the target cannot take `batch`.
    fn check(&self, batch: &DecisionRequest) -> Result<(), ProviderError> {
        // The bare reason: `UnsupportedInput` names the provider itself.
        match self.inner.limits().violation(batch) {
            None => Ok(()),
            Some(reason) => Err(ProviderError::UnsupportedInput {
                provider: self.provider_name.clone(),
                reason,
            }),
        }
    }

    /// `predicate`, `score` and `composite`: questions per document, packed.
    async fn rerank_per_document(
        &self,
        req: &RerankRequest,
        state: Box<RawValue>,
        strategy: &RerankStrategy,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let limits = self.inner.limits();
        let per_doc_questions = strategy.questions_per_doc();
        let mut per_doc: Vec<Vec<Question>> = Vec::with_capacity(req.documents.len());
        let mut costs = Vec::with_capacity(req.documents.len());
        for (i, doc) in req.documents.iter().enumerate() {
            let questions = document_questions(strategy, i, truncate_doc(doc.text()))?;
            costs.push(
                questions
                    .iter()
                    .map(question_tokens)
                    .sum::<Result<u64, ProviderError>>()?,
            );
            per_doc.push(questions);
        }

        let calls = plan_calls(
            tokens::estimate_text(state.get()),
            &costs,
            limits.pack.max_call_tokens,
            docs_per_call(limits, per_doc_questions),
        );
        let mut remaining = per_doc.into_iter();
        let batches: Vec<DecisionRequest> = calls
            .iter()
            .map(|range| {
                let questions: Vec<Question> =
                    remaining.by_ref().take(range.len()).flatten().collect();
                DecisionRequest::new(
                    req.model.clone(),
                    Input::Structured(state.clone()),
                    questions,
                )
            })
            .collect();
        for batch in &batches {
            self.check(batch)?;
        }

        let responses: Vec<DecisionResponse> = stream::iter(batches)
            .map(|batch| self.inner.decide(batch, cancel.clone()))
            .buffered(limits.pack.concurrency.max(1))
            .try_collect()
            .await?;

        let mut results = Vec::with_capacity(req.documents.len());
        let mut usage = UsageSum::default();
        let mut refusals = 0_u32;
        for (range, response) in calls.iter().zip(&responses) {
            usage.add(response);
            if response.answers.len() != range.len() * per_doc_questions {
                return Err(translation(format!(
                    "{} answers for {} questions",
                    response.answers.len(),
                    range.len() * per_doc_questions
                )));
            }
            for (i, answers) in range
                .clone()
                .zip(response.answers.chunks(per_doc_questions))
            {
                results.push(RerankResult {
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                    relevance_score: document_score(strategy, answers, &mut refusals)?,
                    document: None,
                });
            }
        }
        Ok(usage.into_response(results, refusals))
    }

    /// `choice`: one question over every document, one call.
    async fn rerank_choice(
        &self,
        req: &RerankRequest,
        state: Box<RawValue>,
        instructions: &str,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let limits = self.inner.limits();
        match req.documents.len() {
            0 => return Ok(UsageSum::default().into_response(Vec::new(), 0)),
            // A choice of one option needs no model: it is the answer.
            1 => {
                return Ok(UsageSum::default().into_response(
                    vec![RerankResult {
                        index: 0,
                        relevance_score: 1.0,
                        document: None,
                    }],
                    0,
                ))
            }
            n if n > limits.max_choice_options => {
                return Err(ProviderError::UnsupportedInput {
                    provider: self.provider_name.clone(),
                    reason: format!(
                        "a choice rerank of more than {} documents",
                        limits.max_choice_options
                    ),
                });
            }
            _ => {}
        }
        let question = Question {
            name: Some("rank".to_owned()),
            instructions: Some(Text::Plain(instructions.to_owned())),
            kind: QuestionKind::Choice {
                choices: req
                    .documents
                    .iter()
                    .enumerate()
                    .map(|(i, doc)| ChoiceOption {
                        value: ChoiceValue::Str(format!("d{i}")),
                        description: Some(Text::Plain(truncate_doc(doc.text()).to_owned())),
                    })
                    .collect(),
            },
            raw: None,
        };
        if tokens::estimate_text(state.get()) + question_tokens(&question)?
            > limits.pack.max_call_tokens
        {
            return Err(ProviderError::UnsupportedInput {
                provider: self.provider_name.clone(),
                reason: "a choice rerank whose documents do not fit one call".to_owned(),
            });
        }
        let batch =
            DecisionRequest::new(req.model.clone(), Input::Structured(state), vec![question]);
        self.check(&batch)?;
        let response = self.inner.decide(batch, cancel).await?;
        let mut usage = UsageSum::default();
        usage.add(&response);
        let probabilities = match response.answers.first() {
            Some(Answer::Choice { probabilities, .. }) => probabilities,
            Some(Answer::Refusal) => {
                if let Some(hook) = &self.on_refusal {
                    hook(1);
                }
                return Err(ProviderError::ContentFiltered {
                    provider: self.provider_name.clone(),
                    status: 200,
                });
            }
            Some(_) => return Err(translation("expected a choice answer")),
            None => return Err(translation("no answer for the choice")),
        };
        let results = (0..req.documents.len())
            .map(|i| {
                Ok(RerankResult {
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                    relevance_score: unit_score(probabilities.get(i).map_or(0.0, |(_, p)| *p))?,
                    document: None,
                })
            })
            .collect::<Result<_, ProviderError>>()?;
        Ok(usage.into_response(results, 0))
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
    /// Fold in one call's upstream input tokens; a call without usage (or
    /// with zero tokens) makes the total incomplete.
    fn add(&mut self, response: &DecisionResponse) {
        match response.usage {
            Some(u) if u.input_tokens > 0 => self.total = self.total.saturating_add(u.input_tokens),
            _ => self.incomplete = true,
        }
    }

    /// Build the `RerankResponse`; `total_tokens` is 0 (the gateway
    /// estimates) when any call lacked usage.
    fn into_response(self, results: Vec<RerankResult>, refusals: u32) -> RerankResponse {
        RerankResponse {
            results,
            usage: RerankUsage {
                // Decision models bill tokens, not search units: the gateway
                // derives units (ADR 003). A call without usage makes the
                // whole count an estimate rather than a partial upstream sum.
                total_tokens: if self.incomplete { 0 } else { self.total },
                refusals,
                ..RerankUsage::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::family::{encode, wire_ids, FamilyProfile};
    use lumen_core::decisions::DecisionUsage;
    use lumen_core::RerankDocument;
    use std::sync::Mutex;

    #[test]
    fn calls_split_on_document_count_and_token_budget() {
        // 250 cheap documents: split every 100.
        let cheap = vec![10; 250];
        assert_eq!(
            plan_calls(5, &cheap, 48_000, 100),
            vec![0..100, 100..200, 200..250]
        );
        // Big documents: split when the next one would overflow the budget.
        let big = vec![20_000; 5];
        assert_eq!(plan_calls(5, &big, 48_000, 100), vec![0..2, 2..4, 4..5]);
        // A smaller budget splits sooner.
        assert_eq!(
            plan_calls(5, &[6_000; 5], 16_000, 100),
            vec![0..2, 2..4, 4..5]
        );
        // A single document always gets its own call, even if oversized.
        assert_eq!(plan_calls(5, &[60_000], 48_000, 100), vec![0..1]);
        let got = plan_calls(5, &[], 48_000, 100);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn docs_per_call_honours_the_question_limit() {
        let perplexity = FamilyProfile::perplexity().limits;
        assert_eq!(docs_per_call(&perplexity, 1), 100);
        assert_eq!(docs_per_call(&perplexity, 8), 16);
        let cloudflare = FamilyProfile::cloudflare().limits;
        assert_eq!(docs_per_call(&cloudflare, 1), 64);
        assert_eq!(docs_per_call(&cloudflare, 8), 8);
        assert_eq!(docs_per_call(&DecisionLimits::TYPESAFE, 8), 100);
        // A limit below the per-document count still sends one document.
        let tiny = DecisionLimits {
            max_questions: Some(2),
            ..DecisionLimits::TYPESAFE
        };
        assert_eq!(docs_per_call(&tiny, 8), 1);
    }

    #[test]
    fn long_documents_are_truncated_on_a_char_boundary() {
        let text = "é".repeat(20_000); // 40_000 bytes
        let cut = truncate_doc(&text);
        assert!(cut.len() <= 16_384);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(truncate_doc("short"), "short");
    }

    /// A fake decision upstream: records each request as TypeSafe wire JSON
    /// and answers every question with `answer(question name)`.
    struct Fake {
        seen: Mutex<Vec<String>>,
        answer: fn(&str) -> Answer,
        limits: DecisionLimits,
    }

    #[async_trait]
    impl DecisionProvider for Fake {
        async fn decide(
            &self,
            req: DecisionRequest,
            _cancel: CancellationToken,
        ) -> Result<DecisionResponse, ProviderError> {
            let ids = wire_ids(req.questions());
            let bytes = encode(&req, &req.model, &FamilyProfile::typesafe(false), &ids)?;
            self.seen
                .lock()
                .unwrap()
                .push(String::from_utf8(bytes).unwrap());
            Ok(DecisionResponse {
                model: "jev-1".into(),
                answers: ids.iter().map(|id| (self.answer)(id)).collect(),
                usage: Some(DecisionUsage {
                    input_tokens: 10,
                    output_tokens: 1,
                    ..DecisionUsage::default()
                }),
                upstream: None,
            })
        }

        fn limits(&self) -> &DecisionLimits {
            &self.limits
        }

        fn provider_name(&self) -> &'static str {
            "fake"
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
        answer: fn(&str) -> Answer,
    ) -> (DecisionRerankProvider, Arc<Fake>) {
        let fake = Arc::new(Fake {
            seen: Mutex::new(Vec::new()),
            answer,
            limits: DecisionLimits::TYPESAFE,
        });
        let p = DecisionRerankProvider::new(fake.clone(), "typesafe", Arc::new(template));
        (p, fake)
    }

    fn yes(probability: f64) -> Answer {
        Answer::Predicate { probability }
    }

    fn score(score: f64) -> Answer {
        Answer::Score {
            score,
            probabilities: Vec::new(),
            confidence: None,
        }
    }

    #[tokio::test]
    async fn predicate_scores_are_the_probability_and_context_reaches_the_state() {
        let t = RerankTemplate {
            context: Some("legal".into()),
            ..RerankTemplate::default()
        };
        let (p, fake) = provider(t, |id| yes(if id == "0" { 0.9 } else { 0.1 }));
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        let scores: Vec<f32> = out.results.iter().map(|r| r.relevance_score).collect();
        assert_eq!(scores, vec![0.9, 0.1]);
        let seen = fake.seen.lock().unwrap().join("\n");
        assert!(seen.contains(r#""context":"legal""#), "{seen}");
        assert!(seen.contains(r#""type":"noul""#));
        assert_eq!(out.usage.total_tokens, 10);
        assert_eq!(out.usage.refusals, 0);
    }

    #[tokio::test]
    async fn no_context_leaves_the_state_as_the_bare_query() {
        let (p, fake) = provider(RerankTemplate::default(), |_| yes(0.5));
        p.rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap().join("\n");
        assert!(seen.contains(r#""state":{"query":"q"},"#), "{seen}");
    }

    #[tokio::test]
    async fn out_of_range_answers_are_clamped_to_the_unit_interval() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Score {
                instructions: "rate".into(),
                levels: vec!["off".into(), "fully".into()],
            },
        };
        let (p, _) = provider(t, |_| score(7.0));
        let out = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap();
        assert!((out.results[0].relevance_score - 1.0).abs() < f32::EPSILON);

        let (p, _) = provider(RerankTemplate::default(), |_| yes(-0.4));
        let out = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap();
        assert!(out.results[0].relevance_score.abs() < f32::EPSILON);
    }

    #[test]
    fn non_finite_scores_are_a_translation_error() {
        assert!(matches!(
            unit_score(f64::NAN),
            Err(ProviderError::Translation(_))
        ));
        assert!(matches!(
            unit_score(f64::INFINITY),
            Err(ProviderError::Translation(_))
        ));
        assert!((unit_score(0.3).unwrap() - 0.3).abs() < f32::EPSILON);
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
        let (p, fake) = provider(t, |_| score(1.5));
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

    fn hooked(
        template: RerankTemplate,
        answer: fn(&str) -> Answer,
    ) -> (DecisionRerankProvider, Arc<std::sync::atomic::AtomicU64>) {
        let (p, _) = provider(template, answer);
        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen = count.clone();
        let p = p.with_refusal_hook(Arc::new(move |n| {
            seen.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
        }));
        (p, count)
    }

    #[tokio::test]
    async fn a_refused_choice_reports_to_the_hook_and_is_content_filtered() {
        let (p, count) = hooked(choice_template(), |_| Answer::Refusal);
        let err = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::ContentFiltered { status: 200, .. }),
            "{err:?}"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn served_refusals_travel_in_usage_not_the_hook() {
        // Counted once, by the handler from `usage.refusals`.
        let (p, count) = hooked(RerankTemplate::default(), |_| Answer::Refusal);
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.usage.refusals, 2);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_refused_score_is_zero_and_counted() {
        let t = RerankTemplate {
            context: None,
            strategy: RerankStrategy::Score {
                instructions: "rate".into(),
                levels: vec!["off".into(), "fully".into()],
            },
        };
        let (p, _) = provider(t, |id| {
            if id == "0" {
                Answer::Refusal
            } else {
                score(1.0)
            }
        });
        let out = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap();
        let scores: Vec<f32> = out.results.iter().map(|r| r.relevance_score).collect();
        assert_eq!(scores, vec![0.0, 1.0]);
        assert_eq!(out.usage.refusals, 1);
    }

    #[tokio::test]
    async fn a_mistyped_answer_is_a_translation_error() {
        let (p, _) = provider(RerankTemplate::default(), |_| score(1.0));
        let err = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
    }

    #[tokio::test]
    async fn composite_is_the_weighted_mean_of_its_predicates() {
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
        let (p, fake) = provider(t, |id| yes(if id.ends_with(".0") { 1.0 } else { 0.0 }));
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
        for id in [r#""0.0":"#, r#""0.1":"#, r#""1.0":"#, r#""1.1":"#] {
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
        let (p, _) = provider(t, |_| yes(1.0));
        let err = p
            .rerank(request(&["a"]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
    }

    fn choice_answer(_: &str) -> Answer {
        Answer::Choice {
            choice: ChoiceValue::Str("d1".into()),
            probabilities: vec![
                (ChoiceValue::Str("d0".into()), 0.2),
                (ChoiceValue::Str("d1".into()), 0.8),
            ],
            confidence: Some(0.7),
        }
    }

    fn choice_template() -> RerankTemplate {
        RerankTemplate {
            context: None,
            strategy: RerankStrategy::Choice {
                instructions: "best?".into(),
            },
        }
    }

    #[tokio::test]
    async fn choice_uses_one_question_and_option_probabilities() {
        let (p, fake) = provider(choice_template(), choice_answer);
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
    async fn choice_options_are_serialised_in_document_order() {
        let (p, fake) = provider(choice_template(), |_| Answer::Choice {
            choice: ChoiceValue::Str("d0".into()),
            probabilities: Vec::new(),
            confidence: None,
        });
        let docs: Vec<String> = (0..12).map(|i| format!("doc{i}")).collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let out = p
            .rerank(request(&refs), CancellationToken::new())
            .await
            .unwrap();
        // Missing probabilities score 0, never panic.
        assert!(out.results.iter().all(|r| r.relevance_score == 0.0));
        let seen = fake.seen.lock().unwrap()[0].clone();
        let mut last = 0;
        for i in 0..12 {
            let at = seen
                .find(&format!(r#""d{i}":"doc{i}""#))
                .unwrap_or_else(|| panic!("d{i} missing in {seen}"));
            assert!(at >= last, "d{i} out of order in {seen}");
            last = at;
        }
    }

    #[tokio::test]
    async fn choice_over_the_option_limit_is_rejected_before_any_call() {
        let (p, fake) = provider(choice_template(), choice_answer);
        let docs: Vec<String> = (0..256).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let err = p
            .rerank(request(&refs), CancellationToken::new())
            .await
            .unwrap_err();
        match err {
            ProviderError::UnsupportedInput { reason, .. } => {
                assert!(reason.contains("more than 255"), "{reason}");
            }
            other => panic!("expected UnsupportedInput, got {other:?}"),
        }
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_batch_the_target_rejects_is_unsupported_before_any_call() {
        // A target needing two options per choice still serves one document
        // (no call), but a target whose check fails names it.
        let fake = Arc::new(Fake {
            seen: Mutex::new(Vec::new()),
            answer: choice_answer,
            limits: DecisionLimits {
                max_questions: Some(0),
                ..DecisionLimits::TYPESAFE
            },
        });
        let p = DecisionRerankProvider::new(fake.clone(), "tiny", Arc::new(choice_template()));
        let err = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap_err();
        match err {
            ProviderError::UnsupportedInput { provider, reason } => {
                assert_eq!(provider, "tiny");
                assert!(reason.contains("at most 0 questions"), "{reason}");
                // A bare reason: the error's Display names the provider once.
                assert!(!reason.contains("provider"), "{reason}");
            }
            other => panic!("expected UnsupportedInput, got {other:?}"),
        }
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_answer_count_mismatch_is_a_translation_error() {
        struct Short;
        #[async_trait]
        impl DecisionProvider for Short {
            async fn decide(
                &self,
                _req: DecisionRequest,
                _cancel: CancellationToken,
            ) -> Result<DecisionResponse, ProviderError> {
                Ok(DecisionResponse {
                    model: "m".into(),
                    answers: vec![Answer::Predicate { probability: 0.5 }],
                    usage: None,
                    upstream: None,
                })
            }
            fn limits(&self) -> &DecisionLimits {
                &DecisionLimits::TYPESAFE
            }
            fn provider_name(&self) -> &'static str {
                "short"
            }
        }
        let p = DecisionRerankProvider::new(
            Arc::new(Short),
            "short",
            Arc::new(RerankTemplate::default()),
        );
        let err = p
            .rerank(request(&["a", "b"]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
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
        let (p, _) = provider(t.clone(), |_| yes(0.0));
        let debug = format!("{t:?} {:?} {p:?}", t.strategy);
        assert!(!debug.contains("SECRET"), "{debug}");
        assert!(debug.contains("composite"), "{debug}");
        assert_eq!(RerankTemplate::default().strategy.name(), "predicate");
    }
}
