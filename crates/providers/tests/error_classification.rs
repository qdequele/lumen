//! ADR 014: upstream error bodies classify context-length and content-filter
//! refusals through the shared HTTP path.

use lumen_core::ProviderError;
use lumen_providers::http;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn post_json_classifies_a_context_length_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "code": "context_length_exceeded", "message": "too long" }
        })))
        .mount(&server)
        .await;
    let client = http::build_client();
    let err = http::post_json(
        &client,
        &format!("{}/v1/chat/completions", server.uri()),
        &json!({}),
        None,
        "openai",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::ContextLengthExceeded { status: 400, .. }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn post_json_keeps_a_plain_400_as_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "bad" })))
        .mount(&server)
        .await;
    let client = http::build_client();
    let err = http::post_json(
        &client,
        &server.uri(),
        &json!({}),
        None,
        "p",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::Upstream {
                status: 400,
                retryable: false,
                ..
            }
        ),
        "{err:?}"
    );
}
