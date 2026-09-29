//! Wiremock integration tests for the provider API-key check.
//!
//! Every kind that has a free, authenticated, zero-token endpoint is probed
//! there and never on an inference route; the outcome is a tri-state
//! `key_valid` (`Some(true)` / `Some(false)` / `None` = could not tell).
//! Kinds with no such endpoint, keyless kinds, and a missing key never touch
//! the network. The key itself must never appear in the outcome.

use std::time::{Duration, Instant};

use lumen_core::Capability;
use lumen_providers::bedrock::Credentials;
use lumen_providers::{
    BedrockProvider, KeyCheck, ModelSpec, ProviderKind, ProviderSpec, Registry, VertexProvider,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-secret-key-check-xyz";

fn spec(kind: ProviderKind, base_url: Option<String>, api_key: Option<&str>) -> ProviderSpec {
    ProviderSpec {
        name: "p".to_owned(),
        kind,
        api_key: api_key.map(str::to_owned),
        base_url,
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "m".to_owned(),
            upstream_id: "m".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
            rerank_converter: None,
            release_date: None,
        }],
    }
}

fn registry(spec: ProviderSpec) -> Registry {
    Registry::build(vec![spec], reqwest::Client::new(), Duration::from_secs(30))
        .expect("registry builds")
}

async fn check(spec: ProviderSpec) -> KeyCheck {
    let kind = spec.kind;
    let (reported, outcome) = registry(spec)
        .check_key("p", &CancellationToken::new())
        .await
        .expect("provider is configured");
    assert_eq!(reported, kind, "kind comes from the same snapshot");
    outcome
}

/// The key must be absent from every part of the outcome, including Debug.
fn assert_no_secret(outcome: &KeyCheck) {
    let serialized = serde_json::to_string(outcome).expect("serializes");
    assert!(!serialized.contains(KEY), "key leaked: {serialized}");
    let debug = format!("{outcome:?}");
    assert!(!debug.contains(KEY), "key leaked via Debug: {debug}");
}

// --------------------------------------------------------------------------
// Bearer `GET {base}/models` (OpenAI and the OpenAI-compatible hosts)
// --------------------------------------------------------------------------

#[tokio::test]
async fn openai_valid_key_is_reported_valid_via_models_endpoint() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(
        ProviderKind::Openai,
        Some(format!("{}/v1", mock.uri())),
        Some(KEY),
    ))
    .await;

    assert_eq!(outcome.key_valid, Some(true));
    assert_eq!(outcome.reachable, Some(true));
    assert_eq!(outcome.http_status, Some(200));
    assert!(outcome.latency_ms.is_some());
    assert_eq!(
        outcome.endpoint.as_deref(),
        Some(format!("GET {}/v1/models", mock.uri()).as_str())
    );
    assert_no_secret(&outcome);
}

#[tokio::test]
async fn openai_rejected_key_is_reported_invalid() {
    for status in [401, 403] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&mock)
            .await;

        let outcome = check(spec(
            ProviderKind::Openai,
            Some(format!("{}/v1", mock.uri())),
            Some(KEY),
        ))
        .await;

        assert_eq!(outcome.key_valid, Some(false), "HTTP {status}");
        assert_eq!(outcome.reachable, Some(true));
        assert_eq!(outcome.http_status, Some(status));
        assert_no_secret(&outcome);
    }
}

#[tokio::test]
async fn openai_compatible_hosts_and_mistral_probe_models() {
    for kind in [
        ProviderKind::Groq,
        ProviderKind::Together,
        ProviderKind::Fireworks,
        ProviderKind::Deepseek,
        ProviderKind::Xai,
        ProviderKind::Deepinfra,
        ProviderKind::Huggingface,
        ProviderKind::Mistral,
        ProviderKind::Vllm,
    ] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {KEY}").as_str()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let outcome = check(spec(kind, Some(format!("{}/v1", mock.uri())), Some(KEY))).await;
        assert_eq!(outcome.key_valid, Some(true), "{kind:?}");
    }
}

#[tokio::test]
async fn openrouter_probes_its_key_endpoint_because_models_is_public() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/key"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(
        ProviderKind::Openrouter,
        Some(format!("{}/api/v1", mock.uri())),
        Some(KEY),
    ))
    .await;
    assert_eq!(outcome.key_valid, Some(true));
}

#[tokio::test]
async fn cloudflare_probes_the_account_model_search() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/client/v4/accounts/acct123/ai/models/search"))
        .and(query_param("per_page", "1"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(
        ProviderKind::Cloudflare,
        Some(format!("{}/client/v4/accounts/acct123/ai/v1", mock.uri())),
        Some(KEY),
    ))
    .await;
    assert_eq!(outcome.key_valid, Some(true));
}

// --------------------------------------------------------------------------
// Native kinds with their own auth headers
// --------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_probes_models_with_x_api_key_and_version() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(ProviderKind::Anthropic, Some(mock.uri()), Some(KEY))).await;
    assert_eq!(outcome.key_valid, Some(true));
}

#[tokio::test]
async fn cohere_uses_check_api_key_and_honours_its_valid_flag() {
    for (valid, expected) in [(true, Some(true)), (false, Some(false))] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/check-api-key"))
            .and(header("authorization", format!("Bearer {KEY}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"valid": valid})))
            .expect(1)
            .mount(&mock)
            .await;

        let outcome = check(spec(ProviderKind::Cohere, Some(mock.uri()), Some(KEY))).await;
        assert_eq!(outcome.key_valid, expected, "valid={valid}");
    }
}

#[tokio::test]
async fn google_probes_models_and_treats_api_key_invalid_400_as_invalid() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", KEY))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": 400,
                "message": "API key not valid. Please pass a valid API key.",
                "status": "INVALID_ARGUMENT",
                "details": [{ "reason": "API_KEY_INVALID" }]
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(ProviderKind::Google, Some(mock.uri()), Some(KEY))).await;
    assert_eq!(outcome.key_valid, Some(false));
    assert_eq!(outcome.http_status, Some(400));
}

#[tokio::test]
async fn google_unrelated_400_is_not_a_key_verdict() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&mock)
        .await;

    let outcome = check(spec(ProviderKind::Google, Some(mock.uri()), Some(KEY))).await;
    assert_eq!(outcome.key_valid, None);
}

#[tokio::test]
async fn azure_probes_models_with_api_key_header_and_api_version() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/models"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", KEY))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(ProviderKind::Azure, Some(mock.uri()), Some(KEY))).await;
    assert_eq!(outcome.key_valid, Some(true));
}

#[tokio::test]
async fn pinecone_probes_list_indexes_with_api_key_header() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/indexes"))
        .and(header("Api-Key", KEY))
        .and(header("X-Pinecone-API-Version", "2025-01"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = check(spec(ProviderKind::Pinecone, Some(mock.uri()), Some(KEY))).await;
    assert_eq!(outcome.key_valid, Some(true));
}

// --------------------------------------------------------------------------
// No network call: missing key, keyless kinds, kinds with no known endpoint
// --------------------------------------------------------------------------

#[tokio::test]
async fn keyed_kind_without_a_key_is_invalid_without_a_request() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mock)
        .await;

    for key in [None, Some("")] {
        let outcome = check(spec(
            ProviderKind::Openai,
            Some(format!("{}/v1", mock.uri())),
            key,
        ))
        .await;
        assert_eq!(outcome.key_valid, Some(false));
        assert_eq!(outcome.reachable, None);
        assert!(outcome.detail.contains("no API key"), "{}", outcome.detail);
    }
}

#[tokio::test]
async fn keyless_kind_without_a_key_has_nothing_to_check() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mock)
        .await;

    for kind in [ProviderKind::Tei, ProviderKind::Ollama, ProviderKind::Vllm] {
        let outcome = check(spec(kind, Some(mock.uri()), None)).await;
        assert_eq!(outcome.key_valid, None, "{kind:?}");
        assert_eq!(outcome.reachable, None, "{kind:?}");
        assert!(outcome.detail.contains("keyless"), "{}", outcome.detail);
    }
}

#[tokio::test]
async fn kind_without_a_free_check_endpoint_is_unknown_without_a_request() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mock)
        .await;

    for kind in [
        ProviderKind::Jina,
        ProviderKind::Voyage,
        ProviderKind::Mixedbread,
        ProviderKind::Typesafe,
        ProviderKind::Perplexity,
        ProviderKind::Nvidia,
        ProviderKind::Tei,
    ] {
        let outcome = check(spec(kind, Some(mock.uri()), Some(KEY))).await;
        assert_eq!(outcome.key_valid, None, "{kind:?}");
        assert_eq!(outcome.reachable, None, "{kind:?}");
        assert!(
            outcome.detail.contains("no free key-check endpoint"),
            "{kind:?}: {}",
            outcome.detail
        );
    }
}

// --------------------------------------------------------------------------
// Ambiguous outcomes never claim a verdict
// --------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_server_error_and_not_found_are_not_key_verdicts() {
    for status in [429, 404, 500, 503] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&mock)
            .await;

        let outcome = check(spec(
            ProviderKind::Openai,
            Some(format!("{}/v1", mock.uri())),
            Some(KEY),
        ))
        .await;
        assert_eq!(outcome.key_valid, None, "HTTP {status}");
        assert_eq!(outcome.reachable, Some(true), "HTTP {status}");
        assert_eq!(outcome.http_status, Some(status));
    }
}

#[tokio::test]
async fn unreachable_host_is_reported_unreachable() {
    // Bind then drop a listener so the port is (almost certainly) closed.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let outcome = check(spec(
        ProviderKind::Openai,
        Some(format!("http://127.0.0.1:{port}/v1")),
        Some(KEY),
    ))
    .await;
    assert_eq!(outcome.reachable, Some(false));
    assert_eq!(outcome.key_valid, None);
    assert_eq!(outcome.http_status, None);
    assert_no_secret(&outcome);
}

#[tokio::test]
async fn endpoint_label_strips_userinfo_from_base_url() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let with_userinfo = mock.uri().replace("http://", "http://proxyuser:proxypass@");

    let outcome = check(spec(
        ProviderKind::Openai,
        Some(format!("{with_userinfo}/v1")),
        Some(KEY),
    ))
    .await;
    let endpoint = outcome.endpoint.expect("endpoint reported");
    assert!(
        !endpoint.contains("proxypass"),
        "userinfo leaked: {endpoint}"
    );
    assert!(
        !endpoint.contains("proxyuser"),
        "userinfo leaked: {endpoint}"
    );
    assert!(endpoint.ends_with("/v1/models"), "{endpoint}");
}

#[tokio::test]
async fn cancellation_aborts_the_probe_promptly() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&mock)
        .await;

    let reg = registry(spec(
        ProviderKind::Openai,
        Some(format!("{}/v1", mock.uri())),
        Some(KEY),
    ));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });

    let started = Instant::now();
    let (_, outcome) = reg.check_key("p", &cancel).await.expect("configured");
    assert!(started.elapsed() < Duration::from_secs(5), "not aborted");
    assert_eq!(outcome.key_valid, None);
    assert!(outcome.detail.contains("cancelled"), "{}", outcome.detail);
}

// --------------------------------------------------------------------------
// Registry wiring
// --------------------------------------------------------------------------

#[tokio::test]
async fn unknown_provider_name_is_none() {
    let reg = registry(spec(ProviderKind::Openai, None, Some(KEY)));
    assert!(reg
        .check_key("nope", &CancellationToken::new())
        .await
        .is_none());
}

#[tokio::test]
async fn reload_makes_the_check_use_the_rotated_key() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sk-new"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock)
        .await;

    let base = Some(format!("{}/v1", mock.uri()));
    let reg = registry(spec(ProviderKind::Openai, base.clone(), Some("sk-old")));
    let before = reg.check_key("p", &CancellationToken::new()).await;
    assert_eq!(before.map(|(_, o)| o.key_valid), Some(Some(false)));

    reg.reload(vec![spec(ProviderKind::Openai, base, Some("sk-new"))])
        .expect("reload");
    let after = reg.check_key("p", &CancellationToken::new()).await;
    assert_eq!(after.map(|(_, o)| o.key_valid), Some(Some(true)));
}

// --------------------------------------------------------------------------
// Vertex AI: minting an OAuth token proves the service account
// --------------------------------------------------------------------------

const TEST_PEM: &str = include_str!("../src/google/vertex/testdata/test_private_key.pem");

fn vertex(mock: &MockServer) -> VertexProvider {
    let creds = json!({
        "type": "service_account",
        "project_id": "my-project",
        "client_email": "svc@my-project.iam.gserviceaccount.com",
        "private_key": TEST_PEM,
        "token_uri": format!("{}/token", mock.uri()),
    })
    .to_string();
    VertexProvider::new(
        reqwest::Client::new(),
        "vertex",
        Some(&creds),
        None,
        Some("us-central1".to_owned()),
        Some(mock.uri()),
    )
    .expect("provider builds")
}

#[tokio::test]
async fn vertex_token_mint_success_is_valid() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = vertex(&mock).check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(true));
    assert_eq!(outcome.reachable, Some(true));
    let serialized = serde_json::to_string(&outcome).expect("serializes");
    assert!(!serialized.contains("ya29.token"), "token leaked");
}

#[tokio::test]
async fn vertex_token_mint_rejection_is_invalid() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&mock)
        .await;

    let outcome = vertex(&mock).check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(false));
    assert_eq!(outcome.http_status, Some(400));
}

#[tokio::test]
async fn vertex_unusable_private_key_is_invalid_without_a_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mock)
        .await;
    let creds = json!({
        "type": "service_account",
        "project_id": "my-project",
        "client_email": "svc@my-project.iam.gserviceaccount.com",
        "private_key": "-----BEGIN PRIVATE KEY-----\ngarbage\n-----END PRIVATE KEY-----\n",
        "token_uri": format!("{}/token", mock.uri()),
    })
    .to_string();
    let provider = VertexProvider::new(
        reqwest::Client::new(),
        "vertex",
        Some(&creds),
        None,
        Some("us-central1".to_owned()),
        Some(mock.uri()),
    )
    .expect("JSON parses; the key is only used at signing time");

    let outcome = provider.check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(false));
    assert_eq!(outcome.reachable, None);
    assert!(outcome.detail.contains("private key"), "{}", outcome.detail);
}

#[tokio::test]
async fn vertex_malformed_token_response_is_unknown_not_a_fake_502() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&mock)
        .await;

    let outcome = vertex(&mock).check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, None);
    assert_eq!(outcome.reachable, Some(true));
    assert_ne!(outcome.http_status, Some(502), "no upstream 502 happened");
}

#[tokio::test]
async fn vertex_without_credentials_is_invalid_without_a_request() {
    let provider = VertexProvider::new(
        reqwest::Client::new(),
        "vertex",
        None,
        None,
        Some("us-central1".to_owned()),
        None,
    )
    .expect("builds unconfigured");
    let outcome = provider.check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(false));
    assert_eq!(outcome.reachable, None);
}

// --------------------------------------------------------------------------
// Bedrock: a SigV4-signed ListFoundationModels (control plane, zero tokens)
// --------------------------------------------------------------------------

fn bedrock(mock: &MockServer) -> BedrockProvider {
    BedrockProvider::new(
        reqwest::Client::new(),
        "bedrock",
        "us-east-1",
        Some(mock.uri()),
        Some(Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
        )),
    )
}

#[tokio::test]
async fn bedrock_signed_list_foundation_models_success_is_valid() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/foundation-models"))
        .and(body_string(""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"modelSummaries": []})))
        .expect(1)
        .mount(&mock)
        .await;

    let outcome = bedrock(&mock).check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(true));

    let requests = mock.received_requests().await.expect("recorded");
    let auth = requests[0]
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "unsigned: {auth}"
    );
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    let serialized = serde_json::to_string(&outcome).expect("serializes");
    assert!(!serialized.contains("wJalrXUtnFEMI"), "secret leaked");
}

#[tokio::test]
async fn bedrock_access_denied_means_valid_credentials_without_permission() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/foundation-models"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-amzn-ErrorType", "AccessDeniedException:http://internal/")
                .set_body_json(json!({"message": "not authorized"})),
        )
        .mount(&mock)
        .await;

    let outcome = bedrock(&mock).check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(true));
    assert!(
        outcome.detail.contains("bedrock:ListFoundationModels"),
        "{}",
        outcome.detail
    );
}

#[tokio::test]
async fn bedrock_unrecognized_or_bad_signature_is_invalid() {
    for error_type in ["UnrecognizedClientException", "InvalidSignatureException"] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/foundation-models"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-amzn-ErrorType", error_type))
            .mount(&mock)
            .await;

        let outcome = bedrock(&mock).check_key(&CancellationToken::new()).await;
        assert_eq!(outcome.key_valid, Some(false), "{error_type}");
    }
}

#[tokio::test]
async fn bedrock_without_credentials_is_invalid_without_a_request() {
    let provider = BedrockProvider::new(
        reqwest::Client::new(),
        "bedrock",
        "us-east-1",
        Some("http://127.0.0.1:9".to_owned()),
        None,
    );
    let outcome = provider.check_key(&CancellationToken::new()).await;
    assert_eq!(outcome.key_valid, Some(false));
    assert_eq!(outcome.reachable, None);
}
