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
use lumen_core::{
    ContentPart, EmbedInput, EmbedItem, EmbedRequest, EmbeddingProvider, ImageUrl, ProviderError,
};
use lumen_providers::bedrock::{BedrockProvider, Credentials};
use lumen_providers::{
    batch, CohereProvider, GoogleProvider, JinaProvider, MistralProvider, OllamaProvider,
    OpenAiProvider, TeiProvider, VertexProvider, VoyageProvider,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex};
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

    /// The model id the shared scenarios put on the request. Defaults to a
    /// generic name (every fixture that ignores the model keeps working); the
    /// Bedrock fixtures override it so the provider can route by model family
    /// (Titan vs Cohere on Bedrock share one provider but differ on the wire).
    fn model(&self) -> String {
        "test-model".to_owned()
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
            false,
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
// Cohere `input_type` override (issue #22): a caller passes `input_type` as
// an OpenAI-embeddings-request extra field; Cohere honors it verbatim,
// defaulting to `search_document` (the indexing case) when absent.
// --------------------------------------------------------------------------

#[tokio::test]
async fn cohere_embed_defaults_input_type_to_search_document() {
    let mock = MockServer::start().await;
    CohereEmbedFixture.mount_echo(&mock).await;
    let provider = CohereEmbedFixture.build(mock.uri());

    // `CohereEcho` echoes each input parsed as an `f32`; a numeric string
    // input keeps the mock response valid (see `scenario_nominal`).
    let req = batch_request(vec!["0".into()]);
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["input_type"], "search_document");
}

#[tokio::test]
async fn cohere_embed_honors_input_type_override() {
    let mock = MockServer::start().await;
    CohereEmbedFixture.mount_echo(&mock).await;
    let provider = CohereEmbedFixture.build(mock.uri());

    let mut req = batch_request(vec!["0".into()]);
    req.extra
        .insert("input_type".to_owned(), json!("search_query"));
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["input_type"], "search_query");
}

/// Regression (PR #36 review): `EmbedRequest::extra` is a gateway-side carrier
/// (Cohere reads `input_type` from it in Rust); it must NEVER be serialized
/// into an outgoing provider body. The OpenAI-compatible near-passthrough
/// providers (openai, mistral, jina, voyage text paths) serialize the request
/// struct directly, so without `skip_serializing` on `extra` a caller-supplied
/// `input_type` (or any unknown field) would forward verbatim and a strict
/// upstream (vLLM etc.) could reject it.
async fn assert_extra_not_forwarded(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    let mut req = batch_request(vec!["0".into()]);
    req.extra
        .insert("input_type".to_owned(), json!("search_query"));
    req.extra.insert("some_custom_flag".to_owned(), json!(true));
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body.get("input_type").is_none(),
        "`input_type` must not reach a non-Cohere upstream, got body: {body}"
    );
    assert!(
        body.get("some_custom_flag").is_none(),
        "extra fields must not reach the upstream, got body: {body}"
    );
}

#[tokio::test]
async fn openai_embed_does_not_forward_extra_fields() {
    assert_extra_not_forwarded(&OpenAiFixture).await;
}

#[tokio::test]
async fn mistral_embed_does_not_forward_extra_fields() {
    assert_extra_not_forwarded(&MistralEmbedFixture).await;
}

#[tokio::test]
async fn jina_embed_does_not_forward_extra_fields() {
    assert_extra_not_forwarded(&JinaEmbedFixture).await;
}

#[tokio::test]
async fn voyage_embed_does_not_forward_extra_fields() {
    assert_extra_not_forwarded(&VoyageEmbedFixture).await;
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
// Google (Gemini API) fixture - request `requests[].content.parts[].text`,
// response `{ embeddings: [{ values }] }` (+ optional usageMetadata)
// --------------------------------------------------------------------------

struct GoogleEmbedFixture;

struct GoogleEcho;
impl Respond for GoogleEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let empty = Vec::new();
        let requests = body["requests"].as_array().unwrap_or(&empty);
        let embeddings: Vec<Value> = requests
            .iter()
            .map(|r| {
                let text = r["content"]["parts"][0]["text"].as_str().unwrap_or("");
                json!({ "values": [text.parse::<f32>().unwrap_or(f32::NAN)] })
            })
            .collect();
        let n = requests.len();
        ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": embeddings,
            "usageMetadata": { "promptTokenCount": n }
        }))
    }
}

#[async_trait]
impl EmbedFixture for GoogleEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(GoogleProvider::new(
            reqwest::Client::new(),
            "google-test",
            Some(base_url),
            Some("goog-test-key".to_owned()),
        ))
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(GoogleEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({
            "embeddings": [{ "values": [0.0] }],
            "usageMetadata": { "promptTokenCount": 1 }
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
// Vertex AI fixture - request `instances[].content` (`:predict`), response
// `{ predictions: [{ embeddings: { values, statistics } }] }`. The OAuth
// token endpoint lives on its OWN mock server so the shared scenarios'
// received-request counts on the upstream mock stay exact.
// --------------------------------------------------------------------------

const VERTEX_TEST_KEY: &str = include_str!("../src/google/vertex/testdata/test_private_key.pem");

struct VertexEmbedFixture {
    token_server: MockServer,
}

impl VertexEmbedFixture {
    async fn new() -> Self {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ya29.embed-test",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .mount(&token_server)
            .await;
        Self { token_server }
    }
}

struct VertexEcho;
impl Respond for VertexEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let empty = Vec::new();
        let instances = body["instances"].as_array().unwrap_or(&empty);
        let predictions: Vec<Value> = instances
            .iter()
            .map(|i| {
                let text = i["content"].as_str().unwrap_or("");
                json!({
                    "embeddings": {
                        "values": [text.parse::<f32>().unwrap_or(f32::NAN)],
                        "statistics": { "token_count": 1, "truncated": false }
                    }
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({ "predictions": predictions }))
    }
}

#[async_trait]
impl EmbedFixture for VertexEmbedFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        let creds = json!({
            "type": "service_account",
            "project_id": "my-project",
            "client_email": "svc@my-project.iam.gserviceaccount.com",
            "private_key": VERTEX_TEST_KEY,
            "token_uri": format!("{}/token", self.token_server.uri()),
        })
        .to_string();
        Arc::new(
            VertexProvider::new(
                reqwest::Client::new(),
                "vertex-test",
                Some(&creds),
                Some("my-project".to_owned()),
                Some("us-central1".to_owned()),
                Some(base_url),
            )
            .expect("vertex provider builds"),
        )
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(VertexEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({
            "predictions": [{
                "embeddings": {
                    "values": [0.0],
                    "statistics": { "token_count": 1, "truncated": false }
                }
            }]
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
// Bedrock fixtures - `InvokeModel` (`POST /model/{modelId}/invoke`), SigV4.
// Two families share one provider, routed by model id:
//   - Titan: request `{ inputText }` (ONE text per call), response
//     `{ embedding, inputTextTokenCount }`.
//   - Cohere on Bedrock: request `{ texts, input_type }`, response
//     `{ embeddings: [[...]] }` with the input token count in the
//     `x-amzn-bedrock-input-token-count` response header.
// Dummy AWS example credentials sign the requests; the mock never verifies
// the signature.
// --------------------------------------------------------------------------

fn bedrock_creds() -> Credentials {
    Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
    )
}

struct BedrockTitanFixture;

struct BedrockTitanEcho;
impl Respond for BedrockTitanEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let text = body["inputText"].as_str().unwrap_or("");
        let val: f32 = text.parse().unwrap_or(f32::NAN);
        ResponseTemplate::new(200).set_body_json(json!({
            "embedding": [val],
            "inputTextTokenCount": 1
        }))
    }
}

#[async_trait]
impl EmbedFixture for BedrockTitanFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock-titan-test",
            "us-east-1",
            Some(base_url),
            Some(bedrock_creds()),
        ))
    }

    fn model(&self) -> String {
        "amazon.titan-embed-text-v2:0".to_owned()
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(BedrockTitanEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({ "embedding": [0.0], "inputTextTokenCount": 1 });
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

struct BedrockCohereFixture;

struct BedrockCohereEcho;
impl Respond for BedrockCohereEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let inputs = extract_inputs(&body["texts"]);
        let embeddings: Vec<Value> = inputs
            .iter()
            .map(|s| json!([s.parse::<f32>().unwrap_or(f32::NAN)]))
            .collect();
        let n = inputs.len();
        ResponseTemplate::new(200)
            .insert_header("x-amzn-bedrock-input-token-count", n.to_string().as_str())
            .set_body_json(json!({
                "embeddings": embeddings,
                "response_type": "embeddings_floats"
            }))
    }
}

#[async_trait]
impl EmbedFixture for BedrockCohereFixture {
    fn build(&self, base_url: String) -> Arc<dyn EmbeddingProvider> {
        Arc::new(BedrockProvider::new(
            reqwest::Client::new(),
            "bedrock-cohere-test",
            "us-east-1",
            Some(base_url),
            Some(bedrock_creds()),
        ))
    }

    fn model(&self) -> String {
        "cohere.embed-english-v3".to_owned()
    }

    async fn mount_echo(&self, mock: &MockServer) {
        Mock::given(method("POST"))
            .respond_with(BedrockCohereEcho)
            .mount(mock)
            .await;
    }

    async fn mount_delayed(&self, mock: &MockServer, delay: Duration) {
        let body = json!({ "embeddings": [[0.0]], "response_type": "embeddings_floats" });
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amzn-bedrock-input-token-count", "1")
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(mock)
            .await;
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
        extra: serde_json::Map::new(),
    }
}

/// A batch request carrying the fixture's model id, so a provider that routes
/// by model family (Bedrock: Titan vs Cohere) resolves the right wire schema.
fn req_for(fx: &dyn EmbedFixture, texts: Vec<String>) -> EmbedRequest {
    let mut req = batch_request(texts);
    req.model = fx.model();
    req
}

// --------------------------------------------------------------------------
// Scenarios (provider-agnostic)
// --------------------------------------------------------------------------

async fn scenario_nominal(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    let req = req_for(fx, vec!["0".into(), "1".into(), "2".into()]);
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
    let req = req_for(fx, texts);

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
        .embed(req_for(fx, vec!["x".into()]), CancellationToken::new())
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
        .embed(req_for(fx, vec!["x".into()]), CancellationToken::new())
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
        .embed(req_for(fx, vec!["x".into()]), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Translation(_)),
        "malformed body must map to Translation, got {err:?}"
    );
}

async fn scenario_cancellation_aborts_upstream(fx: &dyn EmbedFixture) {
    let mock = MockServer::start().await;
    // Upstream would take 3s; we cancel almost immediately. The delay (and
    // the elapsed bound below) has headroom beyond the bare minimum needed
    // on a quiet host: this suite runs across every provider fixture, and
    // under full workspace-test parallelism (many `#[tokio::test]`s
    // contending for the same CPUs) real-time sleeps and task wakeups can
    // slip by hundreds of ms - this test flaked once under exactly that
    // load. A wider margin keeps the assertion just as meaningful (still
    // asserting the call returns in a small fraction of the mocked delay,
    // proving upstream was never awaited to completion) while tolerating
    // realistic scheduler jitter.
    fx.mount_delayed(&mock, Duration::from_secs(3)).await;
    let provider = fx.build(mock.uri());
    let req = req_for(fx, vec!["x".into()]);

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.embed(req, cancel_child).await });

    // Give the request time to reach the upstream, then cancel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    // Returned well before the 3s upstream delay → the call was aborted.
    assert!(
        elapsed < Duration::from_secs(2),
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

#[tokio::test]
async fn google_passes_embed_conformance_suite() {
    run_conformance(&GoogleEmbedFixture).await;
}

#[tokio::test]
async fn vertex_passes_embed_conformance_suite() {
    let fx = VertexEmbedFixture::new().await;
    run_conformance(&fx).await;
}

#[tokio::test]
async fn bedrock_titan_passes_embed_conformance_suite() {
    run_conformance(&BedrockTitanFixture).await;
}

#[tokio::test]
async fn bedrock_cohere_passes_embed_conformance_suite() {
    run_conformance(&BedrockCohereFixture).await;
}

#[tokio::test]
async fn bedrock_titan_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&BedrockTitanFixture, "bedrock-titan").await;
}

#[tokio::test]
async fn bedrock_cohere_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&BedrockCohereFixture, "bedrock-cohere").await;
}

#[tokio::test]
async fn bedrock_titan_rejects_image_input_before_any_call() {
    assert_rejects_image_input(&BedrockTitanFixture, "bedrock-titan").await;
}

/// Titan `InvokeModel` embeds ONE text per call: the gateway loops one signed
/// request per input, sends `{ inputText }`, maps `dimensions` to Titan v2's
/// own `dimensions`/`normalize` fields, and sums `inputTextTokenCount` as
/// upstream usage (ADR 003, never a silent zero).
#[tokio::test]
async fn bedrock_titan_invokes_once_per_input_with_expected_body_and_usage() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/model/amazon\.titan-embed-text-v2%3A0/invoke$",
        ))
        .respond_with(BedrockTitanEcho)
        .expect(2)
        .mount(&mock)
        .await;
    let provider = BedrockTitanFixture.build(mock.uri());

    let mut req = req_for(&BedrockTitanFixture, vec!["3".into(), "4".into()]);
    req.dimensions = Some(256);
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();

    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].embedding, vec![3.0]);
    assert_eq!(resp.data[1].embedding, vec![4.0]);
    // One token reported per single-input call, summed across the two calls.
    assert_eq!(resp.usage.prompt_tokens, 2);
    assert_eq!(resp.usage.total_tokens, 2);
    assert_eq!(resp.usage.estimated, None);

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        first,
        json!({ "inputText": "3", "dimensions": 256, "normalize": true })
    );
}

/// Cohere on Bedrock embeds a batch in one call: the body carries `texts` and
/// an `input_type` (defaulting to `search_document`, honoring an override), and
/// the input token count comes from the `x-amzn-bedrock-input-token-count`
/// response header (ADR 003 upstream usage).
#[tokio::test]
async fn bedrock_cohere_invokes_batch_with_input_type_and_header_usage() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/cohere\.embed-english-v3/invoke$"))
        .respond_with(BedrockCohereEcho)
        .expect(1)
        .mount(&mock)
        .await;
    let provider = BedrockCohereFixture.build(mock.uri());

    let req = req_for(&BedrockCohereFixture, vec!["5".into(), "6".into()]);
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();

    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].embedding, vec![5.0]);
    assert_eq!(resp.data[1].embedding, vec![6.0]);
    // Header-reported input token count (two texts) surfaces as upstream usage.
    assert_eq!(resp.usage.prompt_tokens, 2);
    assert_eq!(resp.usage.total_tokens, 2);
    assert_eq!(resp.usage.estimated, None);

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "texts": ["5", "6"],
            "input_type": "search_document"
        })
    );
}

#[tokio::test]
async fn bedrock_cohere_honors_input_type_override() {
    let mock = MockServer::start().await;
    BedrockCohereFixture.mount_echo(&mock).await;
    let provider = BedrockCohereFixture.build(mock.uri());

    let mut req = req_for(&BedrockCohereFixture, vec!["0".into()]);
    req.extra
        .insert("input_type".to_owned(), json!("search_query"));
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["input_type"], "search_query");
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

/// Issue #25 review: providers that cannot consume pre-tokenized input must
/// reject it with an honest client error BEFORE any upstream call, instead of
/// sending an empty/garbled body upstream (rule 8: never a misleading error).
async fn assert_rejects_token_input(fx: &dyn EmbedFixture, provider_label: &str) {
    let mock = MockServer::start().await;
    // Mount an echo: if the guard is missing, the call reaches the upstream and
    // the received-requests assertion below catches it.
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    for input in [
        EmbedInput::Tokens(vec![1, 2, 3]),
        EmbedInput::TokenBatch(vec![vec![1, 2], vec![3]]),
    ] {
        let req = EmbedRequest {
            model: "test-model".to_owned(),
            input,
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        let err = provider
            .embed(req, CancellationToken::new())
            .await
            .expect_err("token input must be rejected");
        assert!(
            matches!(err, ProviderError::UnsupportedInput { .. }),
            "{provider_label}: unexpected error {err:?}"
        );
    }
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "{provider_label}: upstream must never be contacted for token input"
    );
}

#[tokio::test]
async fn cohere_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&CohereEmbedFixture, "cohere").await;
}

#[tokio::test]
async fn tei_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&TeiEmbedFixture, "tei").await;
}

#[tokio::test]
async fn ollama_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&OllamaFixture, "ollama").await;
}

#[tokio::test]
async fn jina_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&JinaEmbedFixture, "jina").await;
}

#[tokio::test]
async fn voyage_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&VoyageEmbedFixture, "voyage").await;
}

#[tokio::test]
async fn mistral_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&MistralEmbedFixture, "mistral").await;
}

#[tokio::test]
async fn google_rejects_token_input_before_any_call() {
    assert_rejects_token_input(&GoogleEmbedFixture, "google").await;
}

#[tokio::test]
async fn vertex_rejects_token_input_before_any_call() {
    let fx = VertexEmbedFixture::new().await;
    assert_rejects_token_input(&fx, "vertex").await;
}

/// Gemini and Vertex embeddings are text-only: an image-carrying `Multi` input
/// must be rejected with an honest client error BEFORE any upstream call
/// (rule 8), never forwarded as a garbled body.
async fn assert_rejects_image_input(fx: &dyn EmbedFixture, provider_label: &str) {
    let mock = MockServer::start().await;
    fx.mount_echo(&mock).await;
    let provider = fx.build(mock.uri());

    let req = EmbedRequest {
        model: "test-model".to_owned(),
        input: EmbedInput::Multi(vec![EmbedItem::Parts(vec![ContentPart {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: "data:image/png;base64,QUJD".to_owned(),
                detail: None,
            }),
            extra: serde_json::Map::new(),
        }])]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    let err = provider
        .embed(req, CancellationToken::new())
        .await
        .expect_err("image input must be rejected");
    assert!(
        matches!(err, ProviderError::UnsupportedInput { .. }),
        "{provider_label}: unexpected error {err:?}"
    );
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "{provider_label}: upstream must never be contacted for image input"
    );
}

#[tokio::test]
async fn google_rejects_image_input_before_any_call() {
    assert_rejects_image_input(&GoogleEmbedFixture, "google").await;
}

#[tokio::test]
async fn vertex_rejects_image_input_before_any_call() {
    let fx = VertexEmbedFixture::new().await;
    assert_rejects_image_input(&fx, "vertex").await;
}

/// The google kind must call `models/{model}:batchEmbedContents` with the
/// key in the `x-goog-api-key` header (never the URL), one inner request per
/// input carrying the URL-matching `model` path, and `outputDimensionality`
/// mapped from the OpenAI `dimensions` field.
#[tokio::test]
async fn google_embed_hits_batch_embed_contents_with_expected_body() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-embedding-001:batchEmbedContents",
        ))
        .and(wiremock::matchers::header(
            "x-goog-api-key",
            "goog-test-key",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": [{ "values": [0.1] }, { "values": [0.2] }]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let provider = GoogleEmbedFixture.build(mock.uri());

    let mut req = batch_request(vec!["a".into(), "b".into()]);
    req.model = "gemini-embedding-001".to_owned();
    req.dimensions = Some(256);
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(resp.data.len(), 2);
    // No usageMetadata in this response: zeroed usage, so the gateway derives
    // the ADR-003 estimate at the request edge.
    assert_eq!(resp.usage.prompt_tokens, 0);
    assert_eq!(resp.usage.total_tokens, 0);

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "requests": [
                {
                    "model": "models/gemini-embedding-001",
                    "content": { "parts": [{ "text": "a" }] },
                    "outputDimensionality": 256
                },
                {
                    "model": "models/gemini-embedding-001",
                    "content": { "parts": [{ "text": "b" }] },
                    "outputDimensionality": 256
                }
            ]
        })
    );
    // The key must never appear in the URL.
    assert!(!requests[0].url.as_str().contains("goog-test-key"));
}

/// Issue #90: a batch of MULTI-PART items must issue one inner request per
/// ITEM, never one per text fragment, so no `batchEmbedContents` call ever
/// exceeds Gemini's 100-inner-request ceiling. 150 two-part items = 300 text
/// fragments but only 150 items, so the split is 100 + 50 items (never 200 +
/// 100 fragments in a single call).
#[tokio::test]
async fn google_multipart_batch_never_exceeds_max_batch_in_inner_requests() {
    // A count-only responder: one embedding per `requests[]` entry, so the
    // per-sub-batch length check passes regardless of the (newline-joined,
    // non-numeric) text content.
    struct GoogleCount;
    impl Respond for GoogleCount {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let n = body["requests"].as_array().map_or(0, Vec::len);
            let embeddings: Vec<Value> = (0..n).map(|_| json!({ "values": [0.0] })).collect();
            ResponseTemplate::new(200).set_body_json(json!({ "embeddings": embeddings }))
        }
    }
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(GoogleCount)
        .mount(&mock)
        .await;
    let provider = GoogleEmbedFixture.build(mock.uri());
    assert_eq!(provider.max_batch_size(), 100);

    // 150 items, each carrying two distinct text fragments.
    let items: Vec<EmbedItem> = (0..150)
        .map(|i| {
            EmbedItem::Parts(vec![
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some(format!("item {i} part one")),
                    image_url: None,
                    extra: serde_json::Map::new(),
                },
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("part two".to_owned()),
                    image_url: None,
                    extra: serde_json::Map::new(),
                },
            ])
        })
        .collect();
    let mut req = batch_request(Vec::new());
    req.model = "gemini-embedding-001".to_owned();
    req.input = EmbedInput::Multi(items);

    let resp = batch::embed_batched(&*provider, req, &CancellationToken::new(), 4)
        .await
        .unwrap();

    // One embedding per item (150), never per fragment (would be 300).
    assert_eq!(resp.data.len(), 150);

    // Every upstream call carried at most 100 inner requests.
    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 2, "expected 100 + 50 split");
    for r in &received {
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        let inner = body["requests"].as_array().map_or(0, Vec::len);
        assert!(
            inner <= 100,
            "a batchEmbedContents call carried {inner} inner requests (> 100)"
        );
    }
}

/// The vertex_ai kind must call the regional, project-scoped `:predict`
/// endpoint (Vertex does not expose `batchEmbedContents`) with Bearer auth,
/// `instances[].content` per input, `parameters.outputDimensionality` mapped
/// from `dimensions`, and upstream `statistics.token_count` summed as usage.
#[tokio::test]
async fn vertex_embed_hits_predict_with_expected_body_and_sums_usage() {
    let fx = VertexEmbedFixture::new().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-embedding-001:predict",
        ))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer ya29.embed-test",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "predictions": [
                { "embeddings": { "values": [0.1], "statistics": { "token_count": 3, "truncated": false } } },
                { "embeddings": { "values": [0.2], "statistics": { "token_count": 4, "truncated": false } } }
            ]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let provider = fx.build(mock.uri());

    let mut req = batch_request(vec!["a".into(), "b".into()]);
    req.model = "gemini-embedding-001".to_owned();
    req.dimensions = Some(128);
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].embedding, vec![0.1]);
    assert_eq!(resp.data[1].embedding, vec![0.2]);
    // Upstream-reported usage: summed across predictions, not estimated.
    assert_eq!(resp.usage.prompt_tokens, 7);
    assert_eq!(resp.usage.total_tokens, 7);

    let requests = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "instances": [{ "content": "a" }, { "content": "b" }],
            "parameters": { "outputDimensionality": 128 }
        })
    );
}

/// The OpenAI-compatible passthrough must keep accepting token arrays: they
/// serialize natively as integer arrays and OpenAI consumes them.
#[tokio::test]
async fn openai_token_input_passes_through_natively() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "test-model",
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&mock)
        .await;
    let provider = OpenAiFixture.build(mock.uri());

    let req = EmbedRequest {
        model: "test-model".to_owned(),
        input: EmbedInput::Tokens(vec![1, 2, 3]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(resp.data.len(), 1);

    // The upstream received the raw integer array, untouched.
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["input"], json!([1, 2, 3]));
}

/// Issue #25: in strict mode Ollama rejects `dimensions` (which it cannot honor)
/// with `UnsupportedField` before any upstream call, so it surfaces as 400
/// (`LM-1001`) rather than silently returning full-width vectors.
#[tokio::test]
async fn ollama_strict_mode_rejects_dimensions_before_any_call() {
    let mock = MockServer::start().await;
    // Mount an echo so a non-strict path WOULD succeed; strict must not reach it.
    OllamaFixture.mount_echo(&mock).await;
    let strict = OllamaProvider::new(reqwest::Client::new(), "ollama-strict", mock.uri(), true);

    let req = EmbedRequest {
        model: "test-model".to_owned(),
        input: EmbedInput::Batch(vec!["0".into()]),
        encoding_format: None,
        dimensions: Some(256),
        user: None,
        extra: serde_json::Map::new(),
    };
    let err = strict
        .embed(req, CancellationToken::new())
        .await
        .expect_err("strict mode must reject dimensions");
    assert!(
        matches!(err, ProviderError::UnsupportedField { ref field, .. } if field == "dimensions"),
        "unexpected error: {err:?}"
    );
    // Never reached the upstream.
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// Issue #25: the default (non-strict) mode silently drops `dimensions` and
/// still embeds, preserving backward-compatible behavior.
#[tokio::test]
async fn ollama_non_strict_drops_dimensions_and_embeds() {
    let mock = MockServer::start().await;
    OllamaFixture.mount_echo(&mock).await;
    let provider = OllamaFixture.build(mock.uri());

    let req = EmbedRequest {
        model: "test-model".to_owned(),
        input: EmbedInput::Batch(vec!["0".into(), "1".into()]),
        encoding_format: None,
        dimensions: Some(256),
        user: None,
        extra: serde_json::Map::new(),
    };
    let resp = provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(resp.data.len(), 2);
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}
