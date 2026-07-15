//! Integration tests for the AWS Bedrock provider (Converse API) against a
//! wiremock upstream. These exercise the whole path: SigV4 signing, the
//! Converse round-trip, event-stream streaming from recorded byte fixtures,
//! error classification, cancellation propagation, and secret hygiene.
//!
//! No real credentials are ever used: dummy AWS example keys sign the requests,
//! and the mock never verifies the signature (only that the headers are
//! well-formed and present).

// The frame-builder length casts are bounded by the small fixtures below.
#![allow(clippy::cast_possible_truncation)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError};
use lumen_providers::bedrock::{BedrockProvider, Credentials};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Dummy AWS example credentials (public, not real).
fn creds() -> Credentials {
    Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
    )
}

fn provider(base_url: String) -> BedrockProvider {
    BedrockProvider::new(
        reqwest::Client::new(),
        "bedrock-test",
        "us-east-1",
        Some(base_url),
        Some(creds()),
    )
}

fn user_request() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-3-5-sonnet-20241022-v2:0".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hello".to_owned())),
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

/// Build a raw AWS event-stream frame (`total | headers_len | prelude_crc |
/// headers | payload | message_crc`), CRCs zeroed (the decoder does not check
/// them). String headers only (type 7).
fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7); // string type
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total = 16 + header_bytes.len() + payload.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
    frame(
        &[
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.as_bytes(),
    )
}

// --------------------------------------------------------------------------
// Non-streaming Converse
// --------------------------------------------------------------------------

#[tokio::test]
async fn converse_round_trip_and_signed_headers_are_well_formed() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        // The model id is percent-encoded in the path (colon -> %3A).
        .and(path_regex(r"^/model/.+%3A0/converse$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "Hello there" }
            ] } },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 7, "outputTokens": 2, "totalTokens": 9 }
        })))
        .mount(&mock)
        .await;

    let resp = provider(mock.uri())
        .chat(user_request(), CancellationToken::new())
        .await
        .expect("converse succeeds");

    assert_eq!(resp.object, "chat.completion");
    assert_eq!(
        resp.choices[0]
            .message
            .content
            .as_ref()
            .map(|c| c.text().into_owned()),
        Some("Hello there".to_owned())
    );
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 7);
    assert_eq!(usage.completion_tokens, 2);
    assert_eq!(usage.total_tokens, 9);

    // Inspect the signed request headers.
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let req: &Request = &requests[0];
    let auth = header(req, "authorization");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "authorization malformed: {auth}"
    );
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    assert!(auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date"));
    assert!(auth.contains("Signature="));

    let amz_date = header(req, "x-amz-date");
    // YYYYMMDD'T'HHMMSS'Z' shape.
    assert_eq!(amz_date.len(), 16, "x-amz-date: {amz_date}");
    assert!(amz_date.ends_with('Z') && amz_date.as_bytes()[8] == b'T');

    // x-amz-content-sha256 is the SHA-256 of the exact JSON body.
    let content_sha = header(req, "x-amz-content-sha256");
    assert_eq!(content_sha.len(), 64);
    assert!(content_sha.bytes().all(|b| b.is_ascii_hexdigit()));

    // The body is the Converse schema: no top-level model, message content is
    // a block array.
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert!(body.get("model").is_none());
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 64);
}

fn header<'a>(req: &'a Request, name: &str) -> &'a str {
    req.headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .expect("header is valid utf-8")
}

#[tokio::test]
async fn rate_limited_maps_to_retryable_with_retry_after() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
        .mount(&mock)
        .await;

    let err = provider(mock.uri())
        .chat(user_request(), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimited {
            retry_after,
            provider,
        } => {
            assert_eq!(provider, "bedrock-test");
            assert_eq!(retry_after, Some(Duration::from_secs(3)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn server_error_maps_to_retryable_upstream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let err = provider(mock.uri())
        .chat(user_request(), CancellationToken::new())
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
async fn client_error_maps_to_fatal_upstream() {
    let mock = MockServer::start().await;
    // 403 AccessDenied: a signing/permission failure is the caller's fault.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&mock)
        .await;

    let err = provider(mock.uri())
        .chat(user_request(), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::Upstream {
            status, retryable, ..
        } => {
            assert_eq!(status, 403);
            assert!(!retryable);
        }
        other => panic!("expected fatal Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_body_maps_to_translation_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .mount(&mock)
        .await;

    let err = provider(mock.uri())
        .chat(user_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Translation(_)), "got {err:?}");
}

#[tokio::test]
async fn cancellation_mid_request_aborts_upstream_promptly() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "output": { "message": { "role": "assistant", "content": [{ "text": "x" }] } },
                    "stopReason": "end_turn",
                    "usage": { "inputTokens": 1, "outputTokens": 1 }
                }))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;

    let provider = provider(mock.uri());
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.chat(user_request(), cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation was slow"
    );
}

// --------------------------------------------------------------------------
// Streaming Converse (event-stream byte fixtures)
// --------------------------------------------------------------------------

/// A recorded `converse-stream` response body: concatenated event-stream frames
/// for a short text completion ending with metadata usage.
fn recorded_stream() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&event_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":"Hello"},"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":" world"},"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    body.extend_from_slice(&event_frame(
        "metadata",
        r#"{"usage":{"inputTokens":5,"outputTokens":2,"totalTokens":7}}"#,
    ));
    body
}

#[tokio::test]
async fn converse_stream_translates_frames_to_openai_chunks() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"/converse-stream$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(recorded_stream()),
        )
        .mount(&mock)
        .await;

    let stream = provider(mock.uri())
        .chat_stream(user_request(), CancellationToken::new())
        .await
        .expect("stream opens");
    let chunks: Vec<_> = stream.collect().await;
    let chunks: Vec<_> = chunks.into_iter().map(|c| c.expect("chunk ok")).collect();

    // role, "Hello", " world", finish, usage.
    let text: String = chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.clone())
        .collect();
    assert_eq!(text, "Hello world");

    let finish = chunks
        .iter()
        .find_map(|c| c.choices[0].finish_reason.clone());
    assert_eq!(finish.as_deref(), Some("stop"));

    let usage = chunks.iter().find_map(|c| c.usage).expect("usage chunk");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 2);
    assert_eq!(usage.total_tokens, 7);
}

#[tokio::test]
async fn converse_stream_bytes_emit_sse_frames_terminated_by_done() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(recorded_stream()),
        )
        .mount(&mock)
        .await;

    let stream = provider(mock.uri())
        .chat_stream_bytes(user_request(), CancellationToken::new())
        .await
        .expect("stream opens");
    let bytes: Vec<u8> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flat_map(|b| b.expect("frame ok").to_vec())
        .collect();
    let text = String::from_utf8(bytes).unwrap();

    assert!(text.contains("data: "));
    assert!(text.contains("Hello"));
    // The genuine metadata terminal produces a single [DONE].
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

#[tokio::test]
async fn partial_stream_without_metadata_omits_done() {
    // Client-disconnection / truncated upstream: frames arrive but the metadata
    // terminal never does, so no [DONE] is fabricated (LM-3010 downstream).
    let mock = MockServer::start().await;
    let mut body = Vec::new();
    body.extend_from_slice(&event_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":"partial"},"contentBlockIndex":0}"#,
    ));
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&mock)
        .await;

    let stream = provider(mock.uri())
        .chat_stream_bytes(user_request(), CancellationToken::new())
        .await
        .expect("stream opens");
    let bytes: Vec<u8> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flat_map(|b| b.expect("frame ok").to_vec())
        .collect();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("partial"));
    assert!(
        !text.contains("[DONE]"),
        "no DONE without a metadata terminal"
    );
}

#[tokio::test]
async fn secret_key_never_appears_in_errors_or_debug() {
    let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream detail"))
        .mount(&mock)
        .await;

    let provider = provider(mock.uri());
    // Debug never reveals the secret.
    assert!(!format!("{provider:?}").contains(secret));

    let err = provider
        .chat(user_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(!format!("{err:?}").contains(secret));
    assert!(!err.to_string().contains(secret));
}
