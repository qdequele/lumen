//! Wiremock integration tests for the Google Vertex AI provider.
//!
//! Every test points both the aiplatform endpoint and the OAuth token endpoint
//! at a local wiremock server, so no request ever reaches Google. The service
//! account uses a throwaway RSA key generated only for tests; the token endpoint
//! is mocked and never validates the JWT signature.
//!
//! Coverage: regional endpoint construction, Bearer auth attachment, token
//! caching across requests (token endpoint hit once), token refresh on expiry,
//! non-streaming and streaming chat, cancellation, token-exchange failure
//! surfacing as an upstream (not client-401) error, and secret redaction.

use std::time::Duration;

use futures::StreamExt;
use lumen_core::{
    ChatMessage, ChatProvider, ChatRequest, GatewayError, MessageContent, ProviderError,
};
use lumen_providers::VertexProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = include_str!("../src/google/vertex/testdata/test_private_key.pem");

const TOKEN_PATH: &str = "/token";
const MODEL: &str = "gemini-2.0-flash";

/// The regional, project-scoped path a request for `MODEL` must hit.
fn generate_path(method_suffix: &str) -> String {
    format!(
        "/v1/projects/my-project/locations/us-central1/publishers/google/models/{MODEL}:{method_suffix}"
    )
}

/// Build a provider whose endpoint and token URLs both target `mock`.
fn provider(mock: &MockServer) -> VertexProvider {
    let creds = json!({
        "type": "service_account",
        "project_id": "my-project",
        "client_email": "svc@my-project.iam.gserviceaccount.com",
        "private_key": TEST_KEY,
        "token_uri": format!("{}{TOKEN_PATH}", mock.uri()),
    })
    .to_string();

    VertexProvider::new(
        reqwest::Client::new(),
        "vertex",
        Some(&creds),
        Some("my-project".to_owned()),
        Some("us-central1".to_owned()),
        Some(mock.uri()),
    )
    .expect("provider builds")
}

/// Mount the OAuth token endpoint, returning `access_token` with `expires_in`.
async fn mount_token(mock: &MockServer, access_token: &str, expires_in: u64, expect: u64) {
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access_token,
            "expires_in": expires_in,
            "token_type": "Bearer",
        })))
        .expect(expect)
        .mount(mock)
        .await;
}

/// A minimal Gemini `generateContent` success body.
fn gemini_response(text: &str) -> Value {
    json!({
        "candidates": [{
            "content": { "parts": [{ "text": text }], "role": "model" },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8
        }
    })
}

fn user_request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hi".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

/// Count how many received requests hit a given path.
async fn hits(mock: &MockServer, wanted: &str) -> usize {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == wanted)
        .count()
}

#[tokio::test]
async fn non_streaming_chat_hits_regional_path_with_bearer() {
    let mock = MockServer::start().await;
    mount_token(&mock, "ya29.mock-token", 3600, 1).await;
    Mock::given(method("POST"))
        .and(path(generate_path("generateContent")))
        .and(header("authorization", "Bearer ya29.mock-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response("Hello there")))
        .expect(1)
        .mount(&mock)
        .await;

    let p = provider(&mock);
    let resp = p
        .chat(user_request(), CancellationToken::new())
        .await
        .expect("chat succeeds");

    assert_eq!(
        resp.choices[0]
            .message
            .content
            .as_ref()
            .map(|c| c.text().into_owned()),
        Some("Hello there".to_owned())
    );
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.usage.unwrap().total_tokens, 8);
    // The Bearer-matched mock was hit, proving the token was attached.
    assert_eq!(hits(&mock, &generate_path("generateContent")).await, 1);
}

#[tokio::test]
async fn token_is_cached_across_two_requests() {
    let mock = MockServer::start().await;
    // A long-lived token: the second request must reuse it, so the token
    // endpoint is hit exactly once.
    mount_token(&mock, "ya29.cached", 3600, 1).await;
    Mock::given(method("POST"))
        .and(path(generate_path("generateContent")))
        .and(header("authorization", "Bearer ya29.cached"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response("ok")))
        .expect(2)
        .mount(&mock)
        .await;

    let p = provider(&mock);
    for _ in 0..2 {
        p.chat(user_request(), CancellationToken::new())
            .await
            .expect("chat succeeds");
    }

    assert_eq!(hits(&mock, TOKEN_PATH).await, 1, "token must be cached");
    assert_eq!(hits(&mock, &generate_path("generateContent")).await, 2);
}

#[tokio::test]
async fn token_refreshes_when_expired() {
    let mock = MockServer::start().await;
    // A token that is already within the refresh skew (expires_in 0): each
    // request must mint a fresh one, so the token endpoint is hit twice.
    mount_token(&mock, "ya29.short", 0, 2).await;
    Mock::given(method("POST"))
        .and(path(generate_path("generateContent")))
        .and(header("authorization", "Bearer ya29.short"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response("ok")))
        .expect(2)
        .mount(&mock)
        .await;

    let p = provider(&mock);
    for _ in 0..2 {
        p.chat(user_request(), CancellationToken::new())
            .await
            .expect("chat succeeds");
    }

    assert_eq!(
        hits(&mock, TOKEN_PATH).await,
        2,
        "expired token must refresh"
    );
}

#[tokio::test]
async fn streaming_chat_translates_fragments() {
    let mock = MockServer::start().await;
    mount_token(&mock, "ya29.stream", 3600, 1).await;

    let sse = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({ "candidates": [{ "content": { "parts": [{ "text": "Hel" }], "role": "model" } }] }),
        json!({
            "candidates": [{
                "content": { "parts": [{ "text": "lo" }], "role": "model" },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 1, "totalTokenCount": 3 }
        })
    );
    Mock::given(method("POST"))
        .and(path(generate_path("streamGenerateContent")))
        .and(header("authorization", "Bearer ya29.stream"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let p = provider(&mock);
    let mut req = user_request();
    req.stream = true;
    let stream = p
        .chat_stream(req, CancellationToken::new())
        .await
        .expect("stream opens");
    let chunks: Vec<_> = stream.collect().await;
    let text: String = chunks
        .iter()
        .filter_map(|c| c.as_ref().ok())
        .filter_map(|c| c.choices.first())
        .filter_map(|c| c.delta.content.clone())
        .collect();
    assert_eq!(text, "Hello");
    let finish = chunks
        .iter()
        .filter_map(|c| c.as_ref().ok())
        .filter_map(|c| c.choices.first())
        .find_map(|c| c.finish_reason.clone());
    assert_eq!(finish.as_deref(), Some("stop"));
}

#[tokio::test]
async fn cancellation_aborts_before_any_upstream_call() {
    let mock = MockServer::start().await;
    // Mount both endpoints, but a pre-cancelled token means neither is hit.
    mount_token(&mock, "ya29.never", 3600, 0).await;

    let p = provider(&mock);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = p
        .chat(user_request(), cancel)
        .await
        .expect_err("cancelled request fails");
    assert!(matches!(err, ProviderError::Cancelled));
    assert_eq!(
        hits(&mock, TOKEN_PATH).await,
        0,
        "no token fetch after cancel"
    );
}

#[tokio::test]
async fn token_exchange_failure_is_upstream_never_client_401() {
    let mock = MockServer::start().await;
    // The token endpoint rejects the assertion (403). This must surface as a
    // provider-named upstream error, never a misleading gateway 401.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "invalid_grant"
        })))
        .mount(&mock)
        .await;

    let p = provider(&mock);
    let err = p
        .chat(user_request(), CancellationToken::new())
        .await
        .expect_err("token failure propagates");

    match &err {
        ProviderError::Upstream { provider, .. } => assert_eq!(provider, "vertex"),
        other => panic!("expected Upstream, got {other:?}"),
    }
    let gw = GatewayError::from_provider("vertex", err);
    assert_ne!(gw.http_status(), 401, "must not masquerade as a 401");
    assert_eq!(gw.http_status(), 502);
}

#[tokio::test]
async fn upstream_5xx_propagates_as_retryable() {
    let mock = MockServer::start().await;
    mount_token(&mock, "ya29.ok", 3600, 1).await;
    Mock::given(method("POST"))
        .and(path(generate_path("generateContent")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;

    let p = provider(&mock);
    let err = p
        .chat(user_request(), CancellationToken::new())
        .await
        .expect_err("5xx propagates");
    match err {
        ProviderError::Upstream {
            provider,
            status,
            retryable,
        } => {
            assert_eq!(provider, "vertex");
            assert_eq!(status, 503);
            assert!(retryable);
        }
        other => panic!("expected retryable Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_429_maps_to_rate_limited() {
    let mock = MockServer::start().await;
    mount_token(&mock, "ya29.ok", 3600, 1).await;
    Mock::given(method("POST"))
        .and(path(generate_path("generateContent")))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&mock)
        .await;

    let p = provider(&mock);
    let err = p
        .chat(user_request(), CancellationToken::new())
        .await
        .expect_err("429 propagates");
    match err {
        ProviderError::RateLimited {
            provider,
            retry_after,
        } => {
            assert_eq!(provider, "vertex");
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn debug_output_never_contains_the_service_account_key() {
    let mock = MockServer::start().await;
    let p = provider(&mock);
    let rendered = format!("{p:?}");
    assert!(!rendered.contains("PRIVATE KEY"), "leaked: {rendered}");
    assert!(!rendered.contains("MIIE"), "leaked key body: {rendered}");
    assert!(rendered.contains("<redacted>"));
}
