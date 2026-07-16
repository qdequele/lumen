//! Azure OpenAI provider - wiremock coverage for the Azure-specific delta
//! over the OpenAI wire schema: deployment-routed URLs, `api-version`, the
//! `api-key` auth header, chat (stream + non-stream), embeddings,
//! cancellation, and error mapping.
//!
//! The OpenAI JSON schema itself is exercised end-to-end by the generic
//! conformance suite (`tests/embeddings.rs`); this file focuses on what is
//! unique to Azure: URL construction, auth, and deployment routing via
//! `req.model` (already rewritten to the model's `upstream_id` by the router
//! before the provider is called - see `crates/server/src/chat.rs` /
//! `embeddings.rs`).

use std::time::{Duration, Instant};

use futures::StreamExt;
use lumen_core::{
    ChatMessage, ChatProvider, ChatRequest, EmbedInput, EmbedRequest, EmbeddingProvider,
    MessageContent, ProviderError,
};
use lumen_providers::AzureProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DEPLOYMENT: &str = "my-gpt4o-deployment";
const API_KEY: &str = "sk-test-xxx";

fn chat_request() -> ChatRequest {
    ChatRequest {
        // The client-facing id. The router rewrites this to the resolved
        // `upstream_id` (the Azure deployment name) before calling the
        // provider - tests set it explicitly to mimic that rewrite.
        model: DEPLOYMENT.to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hello".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

fn embed_request() -> EmbedRequest {
    EmbedRequest {
        model: DEPLOYMENT.to_owned(),
        input: EmbedInput::Single("hello".to_owned()),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

fn chat_response_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": DEPLOYMENT,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello from azure" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 }
    })
}

fn embed_response_body() -> Value {
    json!({
        "object": "list",
        "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
        "model": DEPLOYMENT,
        "usage": { "prompt_tokens": 1, "total_tokens": 1 }
    })
}

/// Build an OpenAI-shaped SSE body of `n` chunk frames + `[DONE]`.
fn sse_body(n: usize) -> String {
    use std::fmt::Write as _;

    let mut frames = String::new();
    for i in 0..n {
        let _ = write!(
            frames,
            "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{DEPLOYMENT}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}},\"finish_reason\":null}}]}}\n\n"
        );
    }
    frames.push_str("data: [DONE]\n\n");
    frames
}

// ---------------------------------------------------------------------------
// URL construction, auth header, deployment routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_nominal_hits_deployment_url_with_api_version_and_api_key_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .and(query_param("api-version", "2024-06-01"))
        .and(header("api-key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_body()))
        .mount(&mock)
        .await;

    // The operator overrides api-version via a query string on base_url.
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &format!("{}/?api-version=2024-06-01", mock.uri()),
        None,
        Some(API_KEY.to_owned()),
    );

    let resp = provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        resp.choices[0].message.content.as_ref().unwrap().text(),
        "hello from azure"
    );
    assert_eq!(resp.usage.unwrap().total_tokens, 3);

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    // Never a bearer token: Azure auth is the `api-key` header.
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "must not send an Authorization/bearer header"
    );
}

#[tokio::test]
async fn chat_without_an_explicit_api_version_uses_the_built_in_default() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .and(query_param("api-version", "2024-10-21"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_body()))
        .mount(&mock)
        .await;

    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(), // no ?api-version=... override
        None,        // no first-class api_version either
        Some(API_KEY.to_owned()),
    );

    provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn embed_nominal_hits_the_embeddings_deployment_url() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/openai/deployments/{DEPLOYMENT}/embeddings")))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(embed_response_body()))
        .mount(&mock)
        .await;

    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let resp = provider
        .embed(embed_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].embedding, vec![0.1, 0.2]);
    // The documented Azure/OpenAI embeddings array ceiling.
    assert_eq!(provider.max_batch_size(), 2048);
}

#[tokio::test]
async fn deployment_name_with_reserved_characters_stays_one_encoded_path_segment() {
    let mock = MockServer::start().await;
    // Catch-all: assertions are made on the URL the server actually received.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_body()))
        .mount(&mock)
        .await;

    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    // A hostile/typo'd upstream_id: '/'-'?'-'&'-'#' must not be able to
    // rewrite the URL structure (path traversal, query smuggling, fragment).
    let mut req = chat_request();
    req.model = "my deploy/../x?a=b&c#frag".to_owned();
    provider.chat(req, CancellationToken::new()).await.unwrap();

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        "/openai/deployments/my%20deploy%2F..%2Fx%3Fa%3Db%26c%23frag/chat/completions",
        "the deployment must be exactly one percent-encoded path segment"
    );
    assert_eq!(requests[0].url.query(), Some("api-version=2024-10-21"));
}

#[tokio::test]
async fn base_url_with_extra_unrelated_query_params_still_extracts_api_version() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .and(query_param("api-version", "2024-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_body()))
        .mount(&mock)
        .await;

    // api-version buried among unrelated params (which are NOT forwarded -
    // only api-version is part of the provider's URL contract).
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &format!("{}/?foo=bar&api-version=2024-06-01&baz=1", mock.uri()),
        None,
        Some(API_KEY.to_owned()),
    );

    provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    // Exactly the api-version pair: unrelated base_url params are dropped.
    assert_eq!(requests[0].url.query(), Some("api-version=2024-06-01"));
}

#[tokio::test]
async fn explicit_api_version_field_is_used_when_base_url_has_no_query() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .and(query_param("api-version", "2025-01-01-preview"))
        .and(header("api-key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_body()))
        .mount(&mock)
        .await;

    // The first-class config field, no query string on base_url.
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        Some("2025-01-01-preview".to_owned()),
        Some(API_KEY.to_owned()),
    );

    provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.query(),
        Some("api-version=2025-01-01-preview")
    );
}

#[tokio::test]
async fn explicit_api_version_field_wins_over_the_base_url_query_string() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/openai/deployments/{DEPLOYMENT}/embeddings")))
        .and(query_param("api-version", "2025-01-01-preview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embed_response_body()))
        .mount(&mock)
        .await;

    // Both forms set: the explicit field wins over the base_url query string.
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &format!("{}/?api-version=2023-05-15", mock.uri()),
        Some("2025-01-01-preview".to_owned()),
        Some(API_KEY.to_owned()),
    );

    provider
        .embed(embed_request(), CancellationToken::new())
        .await
        .unwrap();

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.query(),
        Some("api-version=2025-01-01-preview")
    );
}

// ---------------------------------------------------------------------------
// Streaming: passthrough + partial chunks + client disconnection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_stream_forwards_deployment_url_and_upstream_sse_bytes_verbatim() {
    let mock = MockServer::start().await;
    let body = sse_body(20);
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .and(query_param("api-version", "2024-10-21"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.clone()),
        )
        .mount(&mock)
        .await;

    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let mut req = chat_request();
    req.stream = true;
    let stream = provider
        .chat_stream_bytes(req, CancellationToken::new())
        .await
        .unwrap();

    // Zero-copy passthrough (ADR 004): collect every partial byte chunk as it
    // arrives and reassemble - never re-serialized, so the joined bytes are
    // byte-identical to the upstream body.
    let chunks: Vec<bytes::Bytes> = stream.map(Result::unwrap).collect().await;
    let joined: Vec<u8> = chunks.iter().flat_map(|b| b.to_vec()).collect();
    assert_eq!(String::from_utf8(joined).unwrap(), body);

    // The upstream was asked to stream with usage included (ADR 003 hook).
    let requests = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn chat_stream_client_drop_mid_stream_does_not_hang() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{DEPLOYMENT}/chat/completions"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(200)),
        )
        .mount(&mock)
        .await;

    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let mut req = chat_request();
    req.stream = true;

    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = provider
            .chat_stream_bytes(req, CancellationToken::new())
            .await
            .unwrap();
        // Consume only the first frame, then drop the stream mid-flight - the
        // upstream connection must be aborted, never read to completion
        // (client-disconnect lesson, LiteLLM issue #22805).
        assert!(stream.next().await.is_some());
        drop(stream);
    })
    .await;

    assert!(
        outcome.is_ok(),
        "dropping a partially-consumed stream must not hang"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

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

#[tokio::test]
async fn rate_limited_429_maps_to_rate_limited_with_retry_after() {
    let mock = MockServer::start().await;
    mount_status(&mock, 429, Some("11")).await;
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let err = provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(11)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_500_maps_to_retryable_upstream_error() {
    let mock = MockServer::start().await;
    mount_status(&mock, 500, None).await;
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let err = provider
        .embed(embed_request(), CancellationToken::new())
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

#[tokio::test]
async fn malformed_response_body_maps_to_translation_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&mock)
        .await;
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let err = provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Translation(_)),
        "expected Translation, got {err:?}"
    );
}

#[tokio::test]
async fn slow_upstream_beyond_the_client_timeout_maps_to_timeout() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;

    // A short-fused client: the upstream is fine but far too slow.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let provider = AzureProvider::new(
        client,
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let err = provider
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Cancellation aborts the upstream call before it completes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_cancellation_aborts_the_upstream_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.chat(chat_request(), cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn embed_cancellation_aborts_the_upstream_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(embed_response_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        &mock.uri(),
        None,
        Some(API_KEY.to_owned()),
    );

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.embed(embed_request(), cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

// ---------------------------------------------------------------------------
// Secrets never leak into Debug
// ---------------------------------------------------------------------------

#[test]
fn debug_never_shows_the_api_key() {
    let provider = AzureProvider::new(
        reqwest::Client::new(),
        "azure-test",
        "https://my-resource.openai.azure.com",
        None,
        Some(API_KEY.to_owned()),
    );
    let dbg = format!("{provider:?}");
    assert!(!dbg.contains(API_KEY), "leaked: {dbg}");
    assert!(dbg.contains("<redacted>"));
}
