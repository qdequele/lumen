//! Cloudflare Workers AI rerank - provider-specific translation tests.
//!
//! The provider-agnostic conformance suite (`tests/rerank.rs`) already covers
//! ordering, 429, 5xx, malformed bodies and cancellation for
//! `CloudflareRerankProvider`. This file covers behaviour specific to
//! Cloudflare's native `/ai/run/{model}` endpoint: `top_n` -> `top_k`
//! forwarding, the account-root URL derivation, the `{ result, success,
//! errors }` envelope, upstream timeouts, and that no secret ever reaches a
//! log line.

// Exact float equality is intentional: scores round-trip verbatim from a
// mocked JSON body through `serde_json`, no arithmetic happens in between.
#![allow(clippy::float_cmp)]

use std::time::Duration;

use lumen_core::{ProviderError, RerankDocument, RerankProvider, RerankRequest};
use lumen_providers::CloudflareRerankProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const DUMMY_KEY: &str = "sk-test-xxx";

fn request(docs: &[&str], top_n: Option<u32>) -> RerankRequest {
    RerankRequest {
        model: "@cf/baai/bge-reranker-base".to_owned(),
        query: "which is better?".to_owned(),
        documents: docs
            .iter()
            .map(|s| RerankDocument::Text((*s).to_owned()))
            .collect(),
        top_n,
        return_documents: false,
        rank_fields: None,
    }
}

fn success_body() -> Value {
    json!({
        "result": { "response": [{ "id": 0, "score": 0.42 }] },
        "success": true,
        "errors": [],
        "messages": []
    })
}

/// `top_n` on the gateway request must be forwarded as Cloudflare's `top_k`.
#[tokio::test]
async fn top_n_is_forwarded_as_top_k() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    provider
        .rerank(request(&["a", "b"], Some(5)), CancellationToken::new())
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["top_k"], 5);
    assert_eq!(body["query"], "which is better?");
    assert_eq!(body["contexts"], json!([{ "text": "a" }, { "text": "b" }]));
}

/// When the request carries no `top_n`, `top_k` must be omitted entirely
/// rather than serialized as `null` (Cloudflare would reject a null).
#[tokio::test]
async fn missing_top_n_omits_top_k_entirely() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        body.get("top_k").is_none(),
        "top_k must be absent, got {body:?}"
    );
}

/// The request must hit the native `/ai/run/{model}` path, not an
/// OpenAI-compatible or Cohere-shaped `/rerank` path.
#[tokio::test]
async fn request_targets_the_native_ai_run_path() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received[0].url.path(), "/ai/run/@cf/baai/bge-reranker-base");
}

/// A `base_url` configured with the documented `/ai/v1` suffix (used for the
/// OpenAI-compatible chat/embed path) still resolves to the correct account
/// root for the native endpoint - no double `/ai/v1/ai/run` path.
#[tokio::test]
async fn ai_v1_suffixed_base_url_resolves_to_the_account_root() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&mock)
        .await;
    let base_url = format!("{}/ai/v1", mock.uri());
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        base_url,
        Some(DUMMY_KEY.to_owned()),
    );

    provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received[0].url.path(), "/ai/run/@cf/baai/bge-reranker-base");
}

/// Cloudflare's response index (`id`) maps directly to `RerankResult::index`.
#[tokio::test]
async fn response_id_maps_to_result_index() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": { "response": [
                { "id": 1, "score": 0.9 },
                { "id": 0, "score": 0.1 }
            ] },
            "success": true,
            "errors": [],
            "messages": []
        })))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    let resp = provider
        .rerank(request(&["a", "b"], None), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(resp.results[0].index, 1);
    assert_eq!(resp.results[0].relevance_score, 0.9);
    assert_eq!(resp.results[1].index, 0);
    // Cloudflare reports no usage; the gateway derives an ADR-003 estimate.
    assert_eq!(resp.usage.search_units, 0);
}

/// A 2xx response whose envelope carries `success: false` (a body-level
/// failure) must be surfaced as a translation error, not silently accepted as
/// an empty result set.
#[tokio::test]
async fn envelope_success_false_is_a_translation_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "success": false,
            "errors": [{ "code": 5007, "message": "model not found" }],
            "messages": []
        })))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    let err = provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::Translation(msg) => assert!(
            msg.contains("model not found"),
            "expected the upstream error detail in the message, got: {msg}"
        ),
        other => panic!("expected Translation, got {other:?}"),
    }
}

/// An upstream that never responds within the client's timeout must map to
/// `ProviderError::Timeout`, not hang the caller.
#[tokio::test]
async fn upstream_timeout_maps_to_provider_timeout() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&mock)
        .await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let provider =
        CloudflareRerankProvider::new(client, "cf", mock.uri(), Some(DUMMY_KEY.to_owned()));

    let err = provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
}

/// The bearer token must never leak through the request's `Authorization`
/// header logic into `Debug`, and the header itself must be sent as
/// `Bearer <token>` (never in the URL or body).
#[tokio::test]
async fn debug_output_never_contains_the_api_key() {
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1",
        Some("sk-live-do-not-leak-me".to_owned()),
    );
    let debug = format!("{provider:?}");
    assert!(!debug.contains("sk-live-do-not-leak-me"));
}

/// The key is sent as a bearer token, and the request body never echoes it.
#[tokio::test]
async fn api_key_travels_only_in_the_authorization_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&mock)
        .await;
    let provider = CloudflareRerankProvider::new(
        reqwest::Client::new(),
        "cf",
        mock.uri(),
        Some(DUMMY_KEY.to_owned()),
    );

    provider
        .rerank(request(&["a"], None), CancellationToken::new())
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    let auth = received[0]
        .headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .unwrap();
    assert_eq!(auth, format!("Bearer {DUMMY_KEY}"));
    let body_text = String::from_utf8_lossy(&received[0].body);
    assert!(!body_text.contains(DUMMY_KEY));
}
