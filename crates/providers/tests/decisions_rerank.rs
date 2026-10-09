//! Any decision model as a reranker (ADR 014, ADR 016): wire tests for the
//! rerank remap against mocked TypeSafe-family and OpenAI upstreams. The
//! mocks answer every predicate question with the score embedded in its
//! document text (`"text #0.42"`), so the tests check the question shape,
//! the document-to-index mapping, packing per target, usage aggregation,
//! refusals, errors and cancellation.

// Scores round-trip verbatim through JSON; exact equality is intended.
#![allow(clippy::float_cmp)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lumen_core::decisions::{DecisionLimits, DecisionRequest, DecisionResponse};
use lumen_core::{DecisionProvider, ProviderError, RerankDocument, RerankProvider, RerankRequest};
use lumen_providers::decisions::rerank::{
    CompositeQuestion, DecisionRerankProvider, RerankStrategy, RerankTemplate,
};
use lumen_providers::{FamilyDecisionProvider, OpenAiDecisionProvider};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Answers each question with the `#score` suffix of its document, and
/// reports 10 input tokens per question.
struct EchoScores;

impl Respond for EchoScores {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("json body");
        let mut answers = Map::new();
        let questions = body["questions"].as_object().expect("questions object");
        for (id, question) in questions {
            let doc = question["instructions"]["document"]
                .as_str()
                .expect("document");
            let score: f64 = doc
                .rsplit('#')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            answers.insert(id.clone(), json!({"type": "noul", "noul": score}));
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 10 * questions.len(), "output_tokens": questions.len()}
        }))
    }
}

async fn mock_with(responder: impl Respond + 'static) -> MockServer {
    mock_at("/v1/systemone", responder).await
}

async fn mock_at(at: &str, responder: impl Respond + 'static) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(responder)
        .mount(&mock)
        .await;
    mock
}

fn typesafe(uri: String) -> Arc<dyn DecisionProvider> {
    Arc::new(FamilyDecisionProvider::typesafe(
        reqwest::Client::new(),
        "typesafe",
        Some(uri),
        None,
        true,
        Some("ts-test".to_owned()),
    ))
}

fn perplexity(uri: String) -> Arc<dyn DecisionProvider> {
    Arc::new(FamilyDecisionProvider::perplexity(
        reqwest::Client::new(),
        "perplexity",
        uri,
        Some("pplx-test".to_owned()),
    ))
}

fn ollama(uri: String) -> Arc<dyn DecisionProvider> {
    Arc::new(FamilyDecisionProvider::ollama(
        reqwest::Client::new(),
        "ollama",
        uri,
        None,
    ))
}

fn openai(uri: &str) -> Arc<dyn DecisionProvider> {
    Arc::new(OpenAiDecisionProvider::new(
        reqwest::Client::new(),
        "openai",
        Some(format!("{uri}/v1")),
        Some("sk-test".to_owned()),
    ))
}

fn wrap(inner: Arc<dyn DecisionProvider>, template: RerankTemplate) -> DecisionRerankProvider {
    let name = inner.provider_name().to_owned();
    DecisionRerankProvider::new(inner, name, Arc::new(template))
}

fn reranker(uri: String, template: RerankTemplate) -> DecisionRerankProvider {
    wrap(typesafe(uri), template)
}

fn request(docs: &[String]) -> RerankRequest {
    RerankRequest {
        model: "jev-latest".to_owned(),
        query: "capital of France".to_owned(),
        documents: docs.iter().cloned().map(RerankDocument::Text).collect(),
        rank_fields: None,
        top_n: None,
        return_documents: false,
    }
}

#[tokio::test]
async fn one_predicate_per_document_with_the_query_in_the_state() {
    let mock = mock_with(EchoScores).await;
    let template = RerankTemplate {
        context: None,
        strategy: RerankStrategy::Predicate {
            instructions: "Could `document` be the cited precedent?".to_owned(),
            criteria_true: "States the cited rule.".to_owned(),
            criteria_false: "Only a similar topic.".to_owned(),
        },
    };
    let docs = vec![
        "Paris is the capital #0.9".to_owned(),
        "Berlin #0.1".to_owned(),
    ];
    let resp = reranker(mock.uri(), template)
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    let scores: Vec<(u32, f32)> = resp
        .results
        .iter()
        .map(|r| (r.index, r.relevance_score))
        .collect();
    assert_eq!(scores, vec![(0, 0.9), (1, 0.1)]);
    assert_eq!(resp.usage.total_tokens, 20);

    let received = mock.received_requests().await.expect("recorded");
    assert_eq!(received.len(), 1, "two documents fit one call");
    let sent: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(sent["model"], "jev-latest");
    assert_eq!(sent["state"], json!({"query": "capital of France"}));
    assert_eq!(
        sent["questions"]["1"],
        json!({
            "type": "noul",
            "instructions": {
                "document": "Berlin #0.1",
                "question": "Could `document` be the cited precedent?"
            },
            "criteria": {"true": "States the cited rule.", "false": "Only a similar topic."}
        })
    );
}

#[tokio::test]
async fn large_batches_are_split_and_mapped_back_to_original_indices() {
    let mock = mock_with(EchoScores).await;
    let docs: Vec<String> = (0..250).map(|i| format!("doc {i} #0.{i:03}")).collect();
    let resp = reranker(mock.uri(), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    assert_eq!(mock.received_requests().await.expect("recorded").len(), 3);
    assert_eq!(resp.results.len(), 250);
    for result in &resp.results {
        let expected: f32 = format!("0.{:03}", result.index).parse().expect("float");
        assert_eq!(result.relevance_score, expected, "index {}", result.index);
    }
    assert_eq!(resp.usage.total_tokens, 2_500);
}

#[tokio::test]
async fn missing_upstream_usage_leaves_tokens_to_the_gateway_estimate() {
    let mock = mock_with(ResponseTemplate::new(200).set_body_json(json!({
        "model": "jev", "answers": {"0": {"type": "noul", "noul": 0.5}}
    })))
    .await;
    let resp = reranker(mock.uri(), RerankTemplate::default())
        .rerank(request(&["x".to_owned()]), CancellationToken::new())
        .await
        .expect("success");
    assert_eq!(resp.usage.total_tokens, 0);
}

#[tokio::test]
async fn a_missing_answer_is_a_translation_error() {
    let mock =
        mock_with(ResponseTemplate::new(200).set_body_json(json!({"model": "jev", "answers": {}})))
            .await;
    let err = reranker(mock.uri(), RerankTemplate::default())
        .rerank(request(&["x".to_owned()]), CancellationToken::new())
        .await
        .expect_err("no answer");
    assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
}

#[tokio::test]
async fn upstream_errors_keep_their_classification() {
    let mock = mock_with(ResponseTemplate::new(529)).await;
    let err = reranker(mock.uri(), RerankTemplate::default())
        .rerank(request(&["x".to_owned()]), CancellationToken::new())
        .await
        .expect_err("overloaded");
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn cancellation_aborts_every_upstream_call() {
    let mock = mock_with(
        ResponseTemplate::new(200)
            .set_body_json(json!({"model": "jev", "answers": {}}))
            .set_delay(Duration::from_secs(2)),
    )
    .await;
    let provider = reranker(mock.uri(), RerankTemplate::default());
    let docs: Vec<String> = (0..150).map(|i| format!("d{i}")).collect();
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.rerank(request(&docs), child).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let result = handle.await.expect("joined");
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// The `#score` suffix of a document text (0.0 when absent).
fn doc_score(document: &str) -> f64 {
    document
        .rsplit('#')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

fn openai_usage() -> Value {
    json!({"input_tokens": 5, "output_tokens": 1,
           "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
           "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 6})
}

/// Answers every question by OpenAI position with the document's `#score`,
/// reading the document out of the JSON-text instructions. OpenAI renders a
/// predicate's yes/no meanings as a sentence after the instructions, so the
/// JSON object is the first value of the text, not the whole of it.
struct OpenAiEcho;

impl Respond for OpenAiEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let answers: Vec<Value> = body["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| {
                let text = q["instructions"].as_str().unwrap();
                let instructions: Value = serde_json::Deserializer::from_str(text)
                    .into_iter::<Value>()
                    .next()
                    .unwrap()
                    .unwrap();
                let score = doc_score(instructions["document"].as_str().unwrap());
                json!({"type": "predicate", "name": q["name"], "probability": score})
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna", "answers": answers, "usage": openai_usage()
        }))
    }
}

/// Answers an OpenAI request with fixed answers, in order.
struct OpenAiAnswers(Vec<Value>);

impl Respond for OpenAiAnswers {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna", "answers": self.0, "usage": openai_usage()
        }))
    }
}

fn by_score(resp: &lumen_core::RerankResponse) -> Vec<(u32, f32)> {
    let mut ranked: Vec<(u32, f32)> = resp
        .results
        .iter()
        .map(|r| (r.index, r.relevance_score))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}

#[tokio::test]
async fn predicate_rerank_through_an_openai_model() {
    let mock = mock_at("/v1/decisions", OpenAiEcho).await;
    let docs = vec!["a #0.2".to_owned(), "b #0.9".to_owned()];
    let resp = wrap(openai(&mock.uri()), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    assert_eq!(by_score(&resp), vec![(1, 0.9), (0, 0.2)]);
    assert_eq!(resp.usage.total_tokens, 5);
    assert_eq!(resp.usage.refusals, 0);

    let received = mock.received_requests().await.expect("recorded");
    assert_eq!(received.len(), 1);
    let sent: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(sent["model"], "jev-latest");
    // The state travels as compact JSON text (spec 10).
    assert_eq!(sent["input"], r#"{"query":"capital of France"}"#);
    let questions = sent["questions"].as_array().expect("array");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0]["type"], "predicate");
    assert_eq!(questions[1]["name"], "1");
    let text = questions[1]["instructions"].as_str().expect("string");
    assert!(
        text.starts_with(r#"{"document":"b #0.9","question":"Is `document` relevant"#),
        "{text}"
    );
    assert!(text.contains("Answer true if:"), "{text}");
}

#[tokio::test]
async fn predicate_rerank_through_a_perplexity_model() {
    let mock = mock_at("/v1/decisions", EchoScores).await;
    let docs = vec!["a #0.3".to_owned(), "b #0.6".to_owned()];
    let resp = wrap(perplexity(mock.uri()), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    assert_eq!(by_score(&resp), vec![(1, 0.6), (0, 0.3)]);
    assert_eq!(resp.usage.total_tokens, 20);
    let received = mock.received_requests().await.expect("recorded");
    assert_eq!(received.len(), 1);
    let sent: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(sent["state"], json!({"query": "capital of France"}));
    assert_eq!(sent["questions"]["0"]["type"], "noul");
    assert_eq!(sent["questions"]["0"]["instructions"]["document"], "a #0.3");
}

fn composite(criteria: usize) -> RerankTemplate {
    RerankTemplate {
        context: None,
        strategy: RerankStrategy::Composite {
            questions: (0..criteria)
                .map(|c| CompositeQuestion {
                    instructions: format!("criterion {c}?"),
                    criteria_true: "y".into(),
                    criteria_false: "n".into(),
                    weight: 1.0,
                })
                .collect(),
        },
    }
}

#[tokio::test]
async fn composite_packing_respects_the_question_limit() {
    // 8 criteria x 100 documents on Perplexity (128 questions per call):
    // every recorded request carries at most 128 questions, and all 800 are asked.
    let mock = mock_at("/v1/decisions", EchoScores).await;
    let docs: Vec<String> = (0..100).map(|i| format!("doc {i} #0.{i:02}")).collect();
    let resp = wrap(perplexity(mock.uri()), composite(8))
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    let received = mock.received_requests().await.expect("recorded");
    assert!(received.len() > 1, "800 questions cannot share one call");
    let mut ids = std::collections::HashSet::new();
    for r in &received {
        let sent: Value = serde_json::from_slice(&r.body).expect("json");
        let questions = sent["questions"].as_object().expect("object");
        assert!(questions.len() <= 128, "{} questions", questions.len());
        ids.extend(questions.keys().cloned());
    }
    assert_eq!(ids.len(), 800, "every question asked exactly once");
    assert!(ids.contains("0.0") && ids.contains("99.7"));
    assert_eq!(resp.results.len(), 100);
    for r in &resp.results {
        let expected: f32 = format!("0.{:02}", r.index).parse().expect("float");
        assert!(
            (r.relevance_score - expected).abs() < 1e-6,
            "index {}: {}",
            r.index,
            r.relevance_score
        );
    }
    assert_eq!(resp.usage.total_tokens, 8_000);
}

/// Wraps a decision provider and records the most calls in flight at once.
struct Counting {
    inner: Arc<dyn DecisionProvider>,
    now: AtomicUsize,
    max: AtomicUsize,
}

#[async_trait]
impl DecisionProvider for Counting {
    async fn decide(
        &self,
        req: DecisionRequest,
        cancel: CancellationToken,
    ) -> Result<DecisionResponse, ProviderError> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        let out = self.inner.decide(req, cancel).await;
        self.now.fetch_sub(1, Ordering::SeqCst);
        out
    }

    fn limits(&self) -> &DecisionLimits {
        self.inner.limits()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }
}

#[tokio::test]
async fn ollama_runs_one_call_at_a_time_with_a_16k_budget() {
    let mock = mock_at("/v1/systemone", DelayedEcho(Duration::from_millis(40))).await;
    let counting = Arc::new(Counting {
        inner: ollama(mock.uri()),
        now: AtomicUsize::new(0),
        max: AtomicUsize::new(0),
    });
    // 20 documents of about 2,000 estimated tokens: one call at Jev's 48k
    // budget, at least three at Ollama's 16k.
    let docs: Vec<String> = (0..20)
        .map(|i| format!("{} #0.{i:02}", "x".repeat(8_000)))
        .collect();
    let resp = wrap(counting.clone(), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");

    assert_eq!(counting.max.load(Ordering::SeqCst), 1, "one call in flight");
    let received = mock.received_requests().await.expect("recorded");
    assert!(received.len() >= 3, "{} calls", received.len());
    for r in &received {
        let estimated = r.body.len().div_ceil(4);
        assert!(estimated <= 16_000, "a call of ~{estimated} tokens");
    }
    assert_eq!(resp.results.len(), 20);
    for r in &resp.results {
        let expected: f32 = format!("0.{:02}", r.index).parse().expect("float");
        assert!((r.relevance_score - expected).abs() < 1e-6);
    }

    // The same documents fit one call on a 48k target.
    let jev = mock_with(EchoScores).await;
    reranker(jev.uri(), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");
    assert_eq!(jev.received_requests().await.expect("recorded").len(), 1);
}

/// [`EchoScores`] with a response delay, so overlapping calls would overlap.
struct DelayedEcho(Duration);

impl Respond for DelayedEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        EchoScores.respond(request).set_delay(self.0)
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
async fn a_choice_over_one_document_needs_no_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;
    let resp = reranker(mock.uri(), choice_template())
        .rerank(request(&["only".to_owned()]), CancellationToken::new())
        .await
        .expect("success");
    assert_eq!(resp.results.len(), 1);
    assert_eq!(resp.results[0].index, 0);
    assert_eq!(resp.results[0].relevance_score, 1.0);
    assert!(mock.received_requests().await.expect("recorded").is_empty());
}

#[tokio::test]
async fn a_listwise_choice_over_more_than_26_documents_is_unsupported_on_ollama() {
    let mock = mock_at("/v1/systemone", EchoScores).await;
    let docs: Vec<String> = (0..27).map(|i| format!("d{i}")).collect();
    let err = wrap(ollama(mock.uri()), choice_template())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect_err("27 options on Ollama");
    match &err {
        ProviderError::UnsupportedInput { provider, reason } => {
            assert_eq!(provider, "ollama");
            assert!(reason.contains("26"), "{reason}");
            assert!(!reason.contains("provider"), "{reason}");
        }
        other => panic!("expected UnsupportedInput, got {other:?}"),
    }
    assert!(mock.received_requests().await.expect("recorded").is_empty());
}

#[tokio::test]
async fn refusals_score_zero_and_a_refused_choice_is_content_filtered() {
    // Predicate: [refusal, 0.8] -> scores [0.0, 0.8], one refusal.
    let mock = mock_at(
        "/v1/decisions",
        OpenAiAnswers(vec![
            json!({"type": "refusal", "name": "0"}),
            json!({"type": "predicate", "name": "1", "probability": 0.8}),
        ]),
    )
    .await;
    let resp = wrap(openai(&mock.uri()), RerankTemplate::default())
        .rerank(
            request(&["a".to_owned(), "b".to_owned()]),
            CancellationToken::new(),
        )
        .await
        .expect("a refusal is a zero, not an error");
    let scores: Vec<f32> = resp.results.iter().map(|r| r.relevance_score).collect();
    assert_eq!(scores, vec![0.0, 0.8]);
    assert_eq!(resp.usage.refusals, 1);
    // Internal only: never on the wire.
    let wire = serde_json::to_value(&resp).expect("json");
    assert!(wire["usage"].get("refusals").is_none(), "{wire}");

    // Composite: the refused criterion counts 0.0 in the weighted mean.
    let mock = mock_at(
        "/v1/decisions",
        OpenAiAnswers(vec![
            json!({"type": "refusal", "name": "0.0"}),
            json!({"type": "predicate", "name": "0.1", "probability": 0.6}),
        ]),
    )
    .await;
    let resp = wrap(openai(&mock.uri()), composite(2))
        .rerank(request(&["a".to_owned()]), CancellationToken::new())
        .await
        .expect("success");
    assert!(
        (resp.results[0].relevance_score - 0.3).abs() < 1e-6,
        "{}",
        resp.results[0].relevance_score
    );
    assert_eq!(resp.usage.refusals, 1);

    // Choice: a refused listwise question fails the attempt as content_filter.
    let mock = mock_at(
        "/v1/decisions",
        OpenAiAnswers(vec![json!({"type": "refusal", "name": "rank"})]),
    )
    .await;
    let err = wrap(openai(&mock.uri()), choice_template())
        .rerank(
            request(&["a".to_owned(), "b".to_owned()]),
            CancellationToken::new(),
        )
        .await
        .expect_err("refused choice");
    match err {
        ProviderError::ContentFiltered { provider, status } => {
            assert_eq!(provider, "openai");
            assert_eq!(status, 200);
        }
        other => panic!("expected ContentFiltered, got {other:?}"),
    }
}

/// Question count of each recorded call, largest first (calls run
/// concurrently, so arrival order is not call order).
async fn questions_per_call(mock: &MockServer) -> Vec<usize> {
    let mut counts: Vec<usize> = mock
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .map(|r| {
            let sent: Value = serde_json::from_slice(&r.body).expect("json");
            sent["questions"].as_object().expect("object").len()
        })
        .collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    counts
}

#[tokio::test]
async fn token_bound_jev_batching_matches_the_pre_refactor_split() {
    // 60 documents of 3,000 bytes under the default template: the 48k token
    // budget (not the 100-document cap) decides. Pre-refactor Jev rerank
    // (estimate_text of each question body, no id) split [0..56), [56..60).
    let mock = mock_with(EchoScores).await;
    let docs: Vec<String> = (0..60)
        .map(|_| format!("{} #0.5", "x".repeat(2_995)))
        .collect();
    assert_eq!(docs[0].len(), 3_000);
    reranker(mock.uri(), RerankTemplate::default())
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");
    assert_eq!(questions_per_call(&mock).await, vec![56, 4]);
}

#[tokio::test]
async fn composite_jev_batching_rounds_each_question_like_the_pre_refactor_code() {
    // Each question is rounded up to whole tokens on its own: the
    // pre-refactor split is 29 + 29 + 2 documents (rounding a document's
    // questions once would give 30 + 30).
    let mock = mock_with(EchoScores).await;
    let docs: Vec<String> = (0..60)
        .map(|_| format!("{} #0.5", "x".repeat(3_087)))
        .collect();
    let template = RerankTemplate {
        context: None,
        strategy: RerankStrategy::Composite {
            questions: vec![
                CompositeQuestion {
                    instructions: "on topic?".into(),
                    criteria_true: "y".into(),
                    criteria_false: "n".into(),
                    weight: 1.0,
                },
                CompositeQuestion {
                    instructions: "recent?".into(),
                    criteria_true: "y2".into(),
                    criteria_false: "n2".into(),
                    weight: 1.0,
                },
            ],
        },
    };
    reranker(mock.uri(), template)
        .rerank(request(&docs), CancellationToken::new())
        .await
        .expect("success");
    assert_eq!(questions_per_call(&mock).await, vec![58, 58, 4]);
}
