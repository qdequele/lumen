//! The shared provider HTTP client must never follow redirects.
//!
//! reqwest strips `Authorization` on a cross-host redirect, but not the
//! custom auth headers several providers use (`x-api-key`, `api-key`,
//! `x-goog-api-key`, `Api-Key`). A provider API never legitimately
//! redirects, so a 3xx from an upstream (or a stale `base_url`, or a hostile
//! proxy) must surface as an upstream error and the key must never reach the
//! redirect target.

use std::time::Duration;

use lumen_core::{
    Capability, ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError,
};
use lumen_providers::{http, AnthropicProvider, ModelSpec, ProviderKind, ProviderSpec, Registry};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-redirect-secret";

/// An upstream that answers every request with a 302 to `target`, plus the
/// target itself, which must never be hit.
async fn redirecting_pair() -> (MockServer, MockServer) {
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target)
        .await;

    let upstream = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/stolen", target.uri()).as_str()),
        )
        .mount(&upstream)
        .await;
    (upstream, target)
}

async fn assert_key_never_reached(target: &MockServer) {
    let requests = target.received_requests().await.expect("recorded");
    assert!(
        requests.is_empty(),
        "redirect was followed: {} request(s) reached the target",
        requests.len()
    );
}

fn user_request() -> ChatRequest {
    ChatRequest {
        model: "claude".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hi".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(16),
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn shared_client_does_not_forward_x_api_key_across_a_redirect() {
    let (upstream, target) = redirecting_pair().await;
    let provider = AnthropicProvider::new(
        http::build_client(),
        "anthropic",
        Some(upstream.uri()),
        Some(KEY.to_owned()),
    );

    let err = provider
        .chat(user_request(), CancellationToken::new())
        .await
        .expect_err("a redirect is not a success");

    assert!(
        matches!(
            err,
            ProviderError::Upstream {
                status: 302,
                retryable: false,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
    assert_key_never_reached(&target).await;
}

#[tokio::test]
async fn dedicated_per_provider_client_does_not_follow_redirects_either() {
    // `connect_timeout_ms` gives a provider its own client (built by the
    // registry through the same builder): it must share the policy.
    let client = http::build_client_with(Duration::from_millis(500), Duration::from_secs(5));
    let (upstream, target) = redirecting_pair().await;
    let provider = AnthropicProvider::new(
        client,
        "anthropic",
        Some(upstream.uri()),
        Some(KEY.to_owned()),
    );

    let _ = provider
        .chat(user_request(), CancellationToken::new())
        .await;
    assert_key_never_reached(&target).await;
}

#[tokio::test]
async fn key_check_reports_a_redirect_without_following_it() {
    let (upstream, target) = redirecting_pair().await;
    let spec = ProviderSpec {
        name: "pc".to_owned(),
        kind: ProviderKind::Pinecone,
        api_key: Some(KEY.to_owned()),
        base_url: Some(upstream.uri()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "rr".to_owned(),
            upstream_id: "rr".to_owned(),
            capabilities: vec![Capability::Rerank],
            modalities: vec!["text".to_owned()],
            release_date: None,
        }],
        decisions_path: None,
        forward_unknown_fields: None,
    };
    let registry = Registry::build(vec![spec], http::build_client(), Duration::from_secs(30))
        .expect("registry builds");

    let (_, outcome) = registry
        .check_key("pc", &CancellationToken::new())
        .await
        .expect("configured");

    assert_eq!(outcome.key_valid, None);
    assert_eq!(outcome.http_status, Some(302));
    assert!(outcome.detail.contains("redirect"), "{}", outcome.detail);
    assert_key_never_reached(&target).await;
}

#[tokio::test]
async fn credential_free_client_keeps_following_redirects() {
    // Health probes and webhook deliveries carry no provider credential, so
    // they keep the default redirect behaviour (a `301 -> 200` liveness
    // endpoint stays up, a redirecting webhook receiver still gets events).
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&target)
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(301)
                .insert_header("location", format!("{}/moved", target.uri()).as_str()),
        )
        .mount(&upstream)
        .await;

    let client =
        http::build_credential_free_client_with(Duration::from_secs(5), Duration::from_secs(10));
    let response = client
        .get(format!("{}/health", upstream.uri()))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 200, "redirect was not followed");
}
