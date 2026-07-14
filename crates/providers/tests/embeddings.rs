//! Generic embeddings conformance suite (M2 acceptance criterion 6).
//!
//! Every `EmbeddingProvider` must pass the SAME set of scenarios. A fixture
//! knows how to build its provider against a mock server and how to mount
//! responses in that provider's own wire schema; the scenarios themselves are
//! provider-agnostic. New providers implement `EmbedFixture` and get the whole
//! suite for free.

// Exact float equality is intentional: the echo mock returns integer values
// (`i.to_string().parse::<f32>()`) that are exactly representable, so `==`
// verifies both value and ordering precisely. The indices cast to `f32` are
// tiny (< 5000), well within the mantissa.
#![allow(clippy::float_cmp, clippy::cast_precision_loss)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lumen_core::{EmbedInput, EmbedRequest, EmbeddingProvider, ProviderError};
use lumen_providers::{
    batch, CohereProvider, JinaProvider, MistralProvider, OllamaProvider, OpenAiProvider,
    TeiProvider, VoyageProvider,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// --------------------------------------------------------------------------
// Fixture abstraction
// --------------------------------------------------------------------------

#[async_trait]
trait EmbedFixture: Send + Sync {
    /// Build the provider under test, pointed at `base_url`.
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider>;

    /// Mount a success responder that echoes each input's numeric value back
    /// as its embedding (`embedding[0] == input.parse::<f32>()`), so global
    /// ordering can be verified end-to-end.
    async fn mount_echo(&self, mock: &MockServer);

    /// Mount a delayed but valid success (for the cancellation scenario).
    async fn mount_delayed(&self, mock: &MockServer, delay: Duration);

    /// Whether this provider reports token usage. TEI, for instance, returns
    /// no usage, so the batching scenario skips the usage-sum assertion for it
    /// (summing absent usage correctly yields zero).
    fn reports_usage(&self) -> bool {
        true
    }
}

/// Extract the input texts from a request body, tolerating string-or-array.
fn extract_inputs(input: &Value) -> Vec<String> {
    match input {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

// --------------------------------------------------------------------------
// OpenAI fixture
// --------------------------------------------------------------------------

struct OpenAiFixture;

struct OpenAiEcho;
impl Respond for OpenAiEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let inputs = extract_inputs(&body["input"]);
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let val: f32 = s.parse().unwrap_or(f32::NAN);
                json!({ "object": "embedding", "index": i, "embedding": [val] })
            })
            .collect();
        let n = inputs.len();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": data,
            "model": "test-model",
            "usage": { "prompt_tokens": n, "total_tokens": n }
        }))
    }
}

#[async_trait]
impl EmbedFixture for OpenAiFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(OpenAiProvider::new(
            reqwest::Client::new(),
            "openai-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(OpenAiEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.0] }],
            "model": "test-model",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        });
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// Ollama fixture
// --------------------------------------------------------------------------

struct OllamaFixture;

struct OllamaEcho;
impl Respond for OllamaEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let inputs = extract_inputs(&body["input"]);
        let embeddings: Vec<Value> = inputs
            .iter()
            .map(|s| json!([s.parse::<f32>().unwrap_or(f32::NAN)]))
            .collect();
        let n = inputs.len();
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "test-model",
            "embeddings": embeddings,
            "prompt_eval_count": n
        }))
    }
}

#[async_trait]
impl EmbedFixture for OllamaFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(OllamaProvider::new(
            reqwest::Client::new(),
            "ollama-test",
            base_url,
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(OllamaEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({
            "model": "test-model",
            "embeddings": [[0.0]],
            "prompt_eval_count": 1
        });
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// Cohere fixture - request `texts`, response `{ embeddings: { float } }`
// --------------------------------------------------------------------------

struct CohereEmbedFixture;

struct CohereEcho;
impl Respond for CohereEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let inputs = extract_inputs(&body["texts"]);
        let float: Vec<Value> = inputs
            .iter()
            .map(|s| json!([s.parse::<f32>().unwrap_or(f32::NAN)]))
            .collect();
        let n = inputs.len();
        ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": float },
            "meta": { "billed_units": { "input_tokens": n } }
        }))
    }
}

#[async_trait]
impl EmbedFixture for CohereEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(CohereProvider::new(
            reqwest::Client::new(),
            "cohere-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(CohereEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({
            "embeddings": { "float": [[0.0]] },
            "meta": { "billed_units": { "input_tokens": 1 } }
        });
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }
}

// --------------------------------------------------------------------------
// TEI fixture - request `inputs`, response is a bare `[[f32]]` array
// --------------------------------------------------------------------------

struct TeiEmbedFixture;

struct TeiEcho;
impl Respond for TeiEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let inputs = extract_inputs(&body["inputs"]);
        let vectors: Vec<Value> = inputs
            .iter()
            .map(|s| json!([s.parse::<f32>().unwrap_or(f32::NAN)]))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!(vectors))
    }
}

#[async_trait]
impl EmbedFixture for TeiEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(TeiProvider::new(
            reqwest::Client::new(),
            "tei-test",
            base_url,
            None,
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(TeiEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([[0.0]]))
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
    }

    fn reports_usage(&self) -> bool {
        false
    }
}

// --------------------------------------------------------------------------
// Jina & Voyage fixtures - OpenAI-compatible embeddings (reuse `OpenAiEcho`)
// --------------------------------------------------------------------------

struct JinaEmbedFixture;

#[async_trait]
impl EmbedFixture for JinaEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(JinaProvider::new(
            reqwest::Client::new(),
            "jina-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(OpenAiEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        OpenAiFixture.mount_delayed(mock, delay).await;
    }
}

struct MistralEmbedFixture;

#[async_trait]
impl EmbedFixture for MistralEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(MistralProvider::new(
            reqwest::Client::new(),
            "mistral-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(OpenAiEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        OpenAiFixture.mount_delayed(mock, delay).await;
    }
}

struct VoyageEmbedFixture;

#[async_trait]
impl EmbedFixture for VoyageEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(VoyageProvider::new(
            reqwest::Client::new(),
            "voyage-test",
            Some(base_url),
            Some("sk-test-xxx".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(OpenAiEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        OpenAiFixture.mount_delayed(mock, delay).await;
    }
}

// --------------------------------------------------------------------------
// Shared error mounts (schema-agnostic)
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

fn batch_request(texts: Vec<String>) -> EmbedRequest {
    EmbedRequest {
        model: "test-model".to_owned(),
        input: EmbedInput::Batch(texts),
        encoding_format: None,
        dimensions: None,
        user: None,
    }
}

// --------------------------------------------------------------------------
// Scenarios (provider-agnostic)
// --------------------------------------------------------------------------

async fn scenario_nominal(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    let req = batch_request(vec!["0".into(), "1".into(), "2".into()]);
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();

    assert_eq!(resp.data.len(), 3);
    assert_eq!(resp.object, "list");
    for (i, d) in resp.data.iter().enumerate() {
        assert_eq!(d.index as usize, i);
        assert_eq!(d.embedding[0], i as f32);
    }
}

async fn scenario_batching_in_order(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    // N = 2*max + 1 guarantees exactly 3 sub-batches for any provider.
    let max = provider.max_batch_size();
    let n = max * 2 + 1;
    let texts: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    let req = batch_request(texts);

    let resp = batch::embed_batched(&*provider, req, &CancellationToken::new(), 4)
        .await
        .unwrap();

    // Exactly ceil(n/max) == 3 upstream calls.
    let calls = mock.received_requests().await.unwrap().len();
    assert_eq!(calls, 3, "expected 3 sub-batch calls, got {calls}");

    // All embeddings present, re-indexed and in original global order.
    assert_eq!(resp.data.len(), n);
    for (i, d) in resp.data.iter().enumerate() {
        assert_eq!(d.index as usize, i, "index out of order at {i}");
        assert_eq!(d.embedding[0], i as f32, "embedding out of order at {i}");
    }
    // Usage summed across sub-batches (for providers that report it).
    if fx.reports_usage() {
        assert_eq!(resp.usage.total_tokens as usize, n);
    }
}

async fn scenario_rate_limited(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    mount_status(&mock, 429, Some("7")).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .embed(batch_request(vec!["x".into()]), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

async fn scenario_upstream_5xx(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    mount_status(&mock, 500, None).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .embed(batch_request(vec!["x".into()]), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::Upstream {
            status, retryable, ..
        } => {
            assert_eq!(status, 500);
            assert!(retryable);
        }
        other => panic!("expected retryable Upstream, got {other:?}"),
    }
}

async fn scenario_malformed_response(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    mount_malformed(&mock).await;
    let provider = fx.build(mock.uri());

    let err = provider
        .embed(batch_request(vec!["x".into()]), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Translation(_)),
        "malformed body must map to Translation, got {err:?}"
    );
}

async fn scenario_cancellation_aborts_upstream(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    // Upstream would take 2s; we cancel almost immediately.
    fx.mount_delayed(&mock, Duration::from_secs(2)).await;
    let provider = fx.build(mock.uri());

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move {
        provider
            .embed(batch_request(vec!["x".into()]), cancel_child)
            .await
    });

    // Give the request time to reach the upstream, then cancel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    // Returned well before the 2s upstream delay → the call was aborted.
    assert!(
        elapsed < Duration::from_secs(1),
        "cancellation should abort promptly, took {elapsed:?}"
    );
    // The upstream did receive the request (it reached the provider edge).
    let calls = mock.received_requests().await.unwrap().len();
    assert_eq!(calls, 1);
}

/// Run the full suite against one fixture.
async fn run_conformance(fx: &dyn EmbedFixture) {
    scenario_nominal(fx).await;
    scenario_batching_in_order(fx).await;
    scenario_rate_limited(fx).await;
    scenario_upstream_5xx(fx).await;
    scenario_malformed_response(fx).await;
    scenario_cancellation_aborts_upstream(fx).await;
}

// --------------------------------------------------------------------------
// Per-provider entry points - both run the identical suite
// --------------------------------------------------------------------------

#[tokio::test]
async fn openai_passes_conformance_suite() {
    run_conformance(&OpenAiFixture).await;
}

#[tokio::test]
async fn ollama_passes_conformance_suite() {
    run_conformance(&OllamaFixture).await;
}

#[tokio::test]
async fn cohere_passes_embed_conformance_suite() {
    run_conformance(&CohereEmbedFixture).await;
}

#[tokio::test]
async fn tei_passes_embed_conformance_suite() {
    run_conformance(&TeiEmbedFixture).await;
}

#[tokio::test]
async fn jina_passes_embed_conformance_suite() {
    run_conformance(&JinaEmbedFixture).await;
}

#[tokio::test]
async fn voyage_passes_embed_conformance_suite() {
    run_conformance(&VoyageEmbedFixture).await;
}

#[tokio::test]
async fn mistral_passes_embed_conformance_suite() {
    run_conformance(&MistralEmbedFixture).await;
}

/// M2 acceptance criterion 1, verbatim: 5000 inputs, max_batch 2048 → exactly
/// 3 upstream calls, 5000 embeddings in order, usage summed.
#[tokio::test]
async fn openai_5000_inputs_yields_exactly_three_calls_in_order() {
    let fx = OpenAiFixture;
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());
    assert_eq!(provider.max_batch_size(), 2048);

    let texts: Vec<String> = (0..5000).map(|i| i.to_string()).collect();
    let resp = batch::embed_batched(
        &*provider,
        batch_request(texts),
        &CancellationToken::new(),
        4,
    )
    .await
    .unwrap();

    assert_eq!(mock.received_requests().await.unwrap().len(), 3);
    assert_eq!(resp.data.len(), 5000);
    for (i, d) in resp.data.iter().enumerate() {
        assert_eq!(d.index as usize, i);
        assert_eq!(d.embedding[0], i as f32);
    }
    assert_eq!(resp.usage.total_tokens, 5000);
}
