//! Generic reranking conformance suite (M3 acceptance criterion 1).
//!
//! Every `RerankProvider` must pass the SAME scenarios. A fixture knows how to
//! build its provider against a mock server and how to mount responses in that
//! provider's own wire schema; the scenarios are provider-agnostic. New rerank
//! providers implement `RerankFixture` and inherit the whole suite.
//!
//! Error scenarios call `provider.rerank()` directly (translation conformance);
//! the ordering scenario goes through `crate::rerank::rerank`, the gateway-side
//! finaliser that guarantees descending-score order regardless of the upstream.

#![allow(clippy::float_cmp)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lumen_core::{ProviderError, RerankDocument, RerankProvider, RerankRequest};
use lumen_providers::{rerank, CohereProvider, JinaProvider, TeiProvider, VoyageProvider};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

// --------------------------------------------------------------------------
// Fixture abstraction
// --------------------------------------------------------------------------

#[async_trait]
trait RerankFixture: Send + Sync {
    /// Build the provider under test, pointed at `base_url`.
    fn build(&self, base_url: String) -> Arc<dyn RerankProvider>;

    /// Mount a responder that scores exactly three documents deliberately OUT
    /// of order: index 0 → 0.10, index 1 → 0.99, index 2 → 0.50.
    async fn mount_scored(&self, mock: &MockServer);

    /// Mount a delayed but valid success (for the cancellation scenario).
    async fn mount_delayed(&self, mock: &MockServer, delay: Duration);
}

// The three fixed scores every fixture mounts, keyed by original index.
const SCORES: [(u32, f32); 3] = [(0, 0.10), (1, 0.99), (2, 0.50)];

// --------------------------------------------------------------------------
// Cohere fixture - { results: [{index, relevance_score}], meta }
// --------------------------------------------------------------------------

struct CohereFixture;

#[async_trait]
impl RerankFixture for CohereFixture {
    fn build(&self, base_url: String) -> Arc<dyn RerankProvider> {
        Arc::new(CohereProvider::new(
            reqwest::Client::new(),
            "cohere-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_scored(&self, mock: &MockServer) {
        let results: Vec<_> = SCORES
            .iter()
            .map(|(i, s)| json!({ "index": i, "relevance_score": s }))
            .collect();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": results,
                "meta": { "billed_units": { "search_units": 1 } }
            })))
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({
                        "results": [{ "index": 0, "relevance_score": 1.0 }],
                        "meta": { "billed_units": { "search_units": 1 } }
                    }))
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// Jina fixture - { results: [{index, relevance_score}] }
// --------------------------------------------------------------------------

struct JinaFixture;

#[async_trait]
impl RerankFixture for JinaFixture {
    fn build(&self, base_url: String) -> Arc<dyn RerankProvider> {
        Arc::new(JinaProvider::new(
            reqwest::Client::new(),
            "jina-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_scored(&self, mock: &MockServer) {
        let results: Vec<_> = SCORES
            .iter()
            .map(|(i, s)| json!({ "index": i, "relevance_score": s }))
            .collect();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "results": [{ "index": 0, "relevance_score": 1.0 }] }))
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// TEI fixture - bare array [{index, score}]
// --------------------------------------------------------------------------

struct TeiFixture;

#[async_trait]
impl RerankFixture for TeiFixture {
    fn build(&self, base_url: String) -> Arc<dyn RerankProvider> {
        Arc::new(TeiProvider::new(
            reqwest::Client::new(),
            "tei-test",
            base_url,
            None,
        ))
    }

    async fn mount_scored(&self, mock: &MockServer) {
        let results: Vec<_> = SCORES
            .iter()
            .map(|(i, s)| json!({ "index": i, "score": s }))
            .collect();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(results)))
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{ "index": 0, "score": 1.0 }]))
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// Voyage fixture - { data: [{index, relevance_score}] }
// --------------------------------------------------------------------------

struct VoyageFixture;

#[async_trait]
impl RerankFixture for VoyageFixture {
    fn build(&self, base_url: String) -> Arc<dyn RerankProvider> {
        Arc::new(VoyageProvider::new(
            reqwest::Client::new(),
            "voyage-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_scored(&self, mock: &MockServer) {
        let data: Vec<_> = SCORES
            .iter()
            .map(|(i, s)| json!({ "index": i, "relevance_score": s }))
            .collect();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": [{ "index": 0, "relevance_score": 1.0 }] }))
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// Shared error mounts (schema-agnostic) and request helper
// --------------------------------------------------------------------------

async fn mount_status(mock: &MockServer, status: u16, retry_after: Option<&str>) {
    let mut template = ResponseTemplate::new(status);
    if let Some(ra) = retry_after {
        template = template.insert_header("retry-after", ra);
    }
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(mock)
        .await;
}

async fn mount_malformed(mock: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .mount(mock)
        .await;
}

fn request(docs: &[&str]) -> RerankRequest {
    RerankRequest {
        model: "test-model".to_owned(),
        query: "q".to_owned(),
        documents: docs
            .iter()
            .map(|s| RerankDocument::Text((*s).to_owned()))
            .collect(),
        rank_fields: None,
        top_n: None,
        return_documents: false,
    }
}

// --------------------------------------------------------------------------
// Scenarios (provider-agnostic)
// --------------------------------------------------------------------------

async fn scenario_orders_by_descending_score(fx: &dyn RerankFixture) {
    let mock = MockServer::start().await;
    fx.mount_scored(&mock).await;
    let provider = fx.build(mock.uri());

    // Goes through the gateway finaliser so ordering is guaranteed.
    let resp = rerank::rerank(
        &*provider,
        request(&["a", "b", "c"]),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    // Sorted 0.99, 0.50, 0.10 → original indices 1, 2, 0.
    let order: Vec<u32> = resp.results.iter().map(|r| r.index).collect();
    assert_eq!(order, vec![1, 2, 0]);
    assert_eq!(resp.results[0].relevance_score, 0.99);
    // Bandwidth-saving default: no echoed documents.
    assert!(resp.results[0].document.is_none());
}

async fn scenario_rate_limited(fx: &dyn RerankFixture) {
    let mock = MockServer::start().await;
    mount_status(&mock, 429, Some("7")).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .rerank(request(&["x"]), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

async fn scenario_upstream_5xx(fx: &dyn RerankFixture) {
    let mock = MockServer::start().await;
    mount_status(&mock, 503, None).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .rerank(request(&["x"]), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::Upstream {
            status, retryable, ..
        } => {
            assert_eq!(status, 503);
            assert!(retryable);
        }
        other => panic!("expected retryable Upstream, got {other:?}"),
    }
}

async fn scenario_malformed_response(fx: &dyn RerankFixture) {
    let mock = MockServer::start().await;
    mount_malformed(&mock).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .rerank(request(&["x"]), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Translation(_)),
        "malformed body must map to Translation, got {err:?}"
    );
}

async fn scenario_cancellation_aborts_upstream(fx: &dyn RerankFixture) {
    let mock = MockServer::start().await;
    fx.mount_delayed(&mock, Duration::from_secs(2)).await;
    let provider = fx.build(mock.uri());

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.rerank(request(&["x"]), cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "cancellation should abort promptly, took {elapsed:?}"
    );
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

async fn run_conformance(fx: &dyn RerankFixture) {
    scenario_orders_by_descending_score(fx).await;
    scenario_rate_limited(fx).await;
    scenario_upstream_5xx(fx).await;
    scenario_malformed_response(fx).await;
    scenario_cancellation_aborts_upstream(fx).await;
}

// --------------------------------------------------------------------------
// Per-provider entry points - all run the identical suite
// --------------------------------------------------------------------------

#[tokio::test]
async fn cohere_passes_rerank_conformance_suite() {
    run_conformance(&CohereFixture).await;
}

#[tokio::test]
async fn jina_passes_rerank_conformance_suite() {
    run_conformance(&JinaFixture).await;
}

#[tokio::test]
async fn tei_passes_rerank_conformance_suite() {
    run_conformance(&TeiFixture).await;
}

#[tokio::test]
async fn voyage_passes_rerank_conformance_suite() {
    run_conformance(&VoyageFixture).await;
}
