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

/// A raw upstream that answers `400` with a 1000-byte `Content-Length`,
/// writes `partial` of the body and then stalls (never closes).
async fn stalled_400(partial: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let head = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n";
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(partial.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        // Hold the connection open with the body incomplete.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        drop(socket);
    });
    format!("http://{addr}/v1/chat/completions")
}

async fn post_to(url: &str) -> (ProviderError, std::time::Duration) {
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        http::post_json(
            &http::build_client(),
            url,
            &json!({}),
            None,
            "p",
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("a stalled error body must not hold the request")
    .unwrap_err();
    (err, started.elapsed())
}

#[tokio::test]
async fn a_stalled_error_body_falls_back_to_the_status_within_a_second() {
    let url = stalled_400("").await;
    let (err, elapsed) = post_to(&url).await;
    assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
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

#[tokio::test]
async fn a_stalled_error_body_is_classified_from_the_bytes_read_so_far() {
    let url = stalled_400(r#"{"error":{"code":"context_length_exceeded","message":"#).await;
    let (err, elapsed) = post_to(&url).await;
    assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
    assert!(
        matches!(
            err,
            ProviderError::ContextLengthExceeded { status: 400, .. }
        ),
        "{err:?}"
    );
}
