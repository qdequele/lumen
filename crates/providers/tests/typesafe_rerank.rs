//! Jev as a reranker (ADR 013 amendment): wire tests for the rerank to
//! SystemOne converter against a mocked `POST /v1/systemone`. The mock
//! answers every noul question with the score embedded in its document text
//! (`"text #0.42"`), so the tests check the question shape, the
//! document-to-index mapping, batching, usage aggregation, errors and
//! cancellation.

// Scores round-trip verbatim through JSON; exact equality is intended.
#![allow(clippy::float_cmp)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_core::{ProviderError, RerankDocument, RerankProvider, RerankRequest, SystemOneProvider};
use lumen_providers::typesafe::rerank::{RerankConverter, TypesafeRerankProvider};
use lumen_providers::TypesafeProvider;
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
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(responder)
        .mount(&mock)
        .await;
    mock
}

fn reranker(uri: String, converter: RerankConverter) -> TypesafeRerankProvider {
    let inner: Arc<dyn SystemOneProvider> = Arc::new(TypesafeProvider::new(
        reqwest::Client::new(),
        "typesafe",
        Some(uri),
        Some("ts-test".to_owned()),
    ));
    TypesafeRerankProvider::new(inner, "typesafe", converter)
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
async fn one_noul_per_document_with_the_query_in_the_state() {
    let mock = mock_with(EchoScores).await;
    let converter = RerankConverter {
        instructions: "Could `document` be the cited precedent?".to_owned(),
        criteria_true: "States the cited rule.".to_owned(),
        criteria_false: "Only a similar topic.".to_owned(),
    };
    let docs = vec![
        "Paris is the capital #0.9".to_owned(),
        "Berlin #0.1".to_owned(),
    ];
    let resp = reranker(mock.uri(), converter)
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
    let resp = reranker(mock.uri(), RerankConverter::default())
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
    let resp = reranker(mock.uri(), RerankConverter::default())
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
    let err = reranker(mock.uri(), RerankConverter::default())
        .rerank(request(&["x".to_owned()]), CancellationToken::new())
        .await
        .expect_err("no answer");
    assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
}

#[tokio::test]
async fn upstream_errors_keep_their_classification() {
    let mock = mock_with(ResponseTemplate::new(529)).await;
    let err = reranker(mock.uri(), RerankConverter::default())
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
    let provider = reranker(mock.uri(), RerankConverter::default());
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
