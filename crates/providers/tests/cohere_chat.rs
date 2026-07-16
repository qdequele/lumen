//! Cohere v2 chat (`POST /v2/chat`) conformance: nominal request/response,
//! upstream error classification (429/500), timeout, cancellation, and
//! streaming (partial-chunk translation + client disconnection aborting the
//! upstream connection).
//!
//! Written wiremock-first (M4/M9 house style, see `tests/rerank.rs`): every
//! scenario here failed to compile/pass before `crates/providers/src/cohere/`
//! grew a `ChatProvider` implementation.

// Building a fixture SSE body is clearest with format!; the style lints don't
// earn their keep in test scaffolding (matches `server/tests/chat.rs`).
#![allow(clippy::format_collect)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use lumen_core::{
    ChatMessage, ChatProvider, ChatRequest, ContentPart, ImageUrl, MessageContent, ProviderError,
};
use lumen_providers::CohereProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DUMMY_KEY: &str = "sk-test-xxx";

fn provider(base_url: String) -> CohereProvider {
    CohereProvider::new(
        reqwest::Client::new(),
        "cohere-test",
        Some(base_url),
        Some(DUMMY_KEY.to_owned()),
    )
}

fn request(model: &str, content: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text(content.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream,
        extra: serde_json::Map::new(),
    }
}

/// A vision request (issue #73): one user message carrying a text part, an
/// inline `data:` URI image (with a `detail` hint) and a remote URL image.
fn vision_request(model: &str, stream: bool) -> ChatRequest {
    let text_part = ContentPart {
        kind: "text".to_owned(),
        text: Some("what is this?".to_owned()),
        image_url: None,
        extra: serde_json::Map::new(),
    };
    let inline_image = ContentPart {
        kind: "image_url".to_owned(),
        text: None,
        image_url: Some(ImageUrl {
            url: "data:image/png;base64,AAAA".to_owned(),
            detail: Some("low".to_owned()),
        }),
        extra: serde_json::Map::new(),
    };
    let remote_image = ContentPart {
        kind: "image_url".to_owned(),
        text: None,
        image_url: Some(ImageUrl {
            url: "https://example.com/cat.png".to_owned(),
            detail: None,
        }),
        extra: serde_json::Map::new(),
    };
    ChatRequest {
        model: model.to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                text_part,
                inline_image,
                remote_image,
            ])),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream,
        extra: serde_json::Map::new(),
    }
}

fn nominal_body() -> Value {
    json!({
        "id": "chat_abc123",
        "finish_reason": "COMPLETE",
        "message": {
            "role": "assistant",
            "content": [{ "type": "text", "text": "Bonjour!" }]
        },
        "usage": {
            "billed_units": { "input_tokens": 9, "output_tokens": 4 },
            "tokens": { "input_tokens": 10, "output_tokens": 5 }
        }
    })
}

// --------------------------------------------------------------------------
// Non-streaming: nominal
// --------------------------------------------------------------------------

#[tokio::test]
async fn nominal_non_streaming_request_translates_both_ways() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(nominal_body()))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let resp = p
        .chat(
            request("command-r-plus", "salut", false),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(resp.object, "chat.completion");
    assert_eq!(resp.id, "chat_abc123");
    assert_eq!(
        resp.choices[0].message.content.as_ref().unwrap().text(),
        "Bonjour!"
    );
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    // `tokens` (real counts) wins over `billed_units`.
    let usage = resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.total_tokens, 15);
    assert_eq!(usage.estimated, None);

    // The outgoing request: system stays inline, `stream` omitted (false).
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "command-r-plus");
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["messages"][0]["content"], "salut");
    assert!(sent.get("stream").is_none());

    // The bearer key reached the upstream request but never a client error.
    let auth = mock.received_requests().await.unwrap()[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(auth, format!("Bearer {DUMMY_KEY}"));
}

/// Issue #72: `response_format` and `seed` map natively onto Cohere v2's own
/// fields instead of being silently dropped - proven on the actual wire body.
#[tokio::test]
async fn response_format_and_seed_reach_the_cohere_wire() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(nominal_body()))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let mut req = request("command-r-plus", "as json please", false);
    req.extra.insert(
        "response_format".to_owned(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "city",
                "schema": { "type": "object", "properties": { "name": { "type": "string" } } }
            }
        }),
    );
    req.extra.insert("seed".to_owned(), json!(42));
    p.chat(req, CancellationToken::new()).await.unwrap();

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["response_format"]["type"], "json_object");
    assert_eq!(sent["response_format"]["json_schema"]["type"], "object");
    assert_eq!(sent["seed"], 42);
}

/// Issue #72: strict mode turns fields the v2 translation cannot honor into
/// an honest pre-flight rejection - the mock records ZERO upstream requests.
#[tokio::test]
async fn strict_mode_rejects_logprobs_before_any_upstream_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(nominal_body()))
        .mount(&mock)
        .await;

    let p = provider(mock.uri()).with_strict(true);
    let mut req = request("command-r-plus", "hi", false);
    req.extra.insert("logprobs".to_owned(), json!(true));
    let err = p.chat(req, CancellationToken::new()).await.unwrap_err();

    assert!(
        matches!(
            &err,
            ProviderError::UnsupportedField { provider, field }
                if provider == "cohere-test" && field == "logprobs"
        ),
        "expected UnsupportedField, got {err:?}"
    );
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "strict rejection must happen before any upstream call"
    );
}

#[tokio::test]
async fn missing_usage_leaves_none_for_the_gateway_to_estimate() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat_no_usage",
            "finish_reason": "COMPLETE",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "hi" }] }
        })))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let resp = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
        .await
        .unwrap();
    assert!(resp.usage.is_none());
}

// --------------------------------------------------------------------------
// Non-streaming: upstream error classification
// --------------------------------------------------------------------------

#[tokio::test]
async fn rate_limited_429_is_classified_with_retry_after() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let err = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(3)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_500_is_retryable() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let err = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
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
async fn client_fault_4xx_is_not_retryable() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let err = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProviderError::Upstream {
            status, retryable, ..
        } => {
            assert_eq!(status, 401);
            assert!(!retryable);
        }
        other => panic!("expected fatal Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_response_body_is_a_translation_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let err = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Translation(_)));
}

// --------------------------------------------------------------------------
// Timeout
// --------------------------------------------------------------------------

#[tokio::test]
async fn slow_upstream_times_out() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(nominal_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;

    let short_timeout_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let p = CohereProvider::new(
        short_timeout_client,
        "cohere-test",
        Some(mock.uri()),
        Some(DUMMY_KEY.to_owned()),
    );

    let started = Instant::now();
    let err = p
        .chat(request("command-r", "hi", false), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Timeout { .. }));
    assert!(started.elapsed() < Duration::from_secs(1));
}

// --------------------------------------------------------------------------
// Cancellation mid-request (non-streaming)
// --------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_aborts_the_in_flight_non_streaming_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(nominal_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let req = request("command-r", "hi", false);
    let started = Instant::now();
    let handle = tokio::spawn(async move { p.chat(req, cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation should abort promptly"
    );
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

// --------------------------------------------------------------------------
// Vision (issue #73): image parts become Cohere v2 content blocks
// --------------------------------------------------------------------------

/// Issue #73: image parts must reach the wire as Cohere v2 content blocks
/// (`{"type":"text",...}` / `{"type":"image_url","image_url":{...}}`), order
/// preserved, URL form (`data:` URI or remote URL) and `detail` untouched -
/// not flattened to text.
#[tokio::test]
async fn image_parts_are_sent_as_cohere_v2_content_blocks() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(nominal_body()))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let resp = p
        .chat(
            vision_request("command-a-vision-07-2025", false),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Upstream usage stays authoritative for a vision request (ADR 003).
    let usage = resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.estimated, None);

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    let content = sent["messages"][0]["content"]
        .as_array()
        .expect("content must be an array of blocks, not flattened text");
    assert_eq!(content.len(), 3);
    assert_eq!(
        content[0],
        json!({ "type": "text", "text": "what is this?" })
    );
    assert_eq!(
        content[1],
        json!({
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,AAAA", "detail": "low" }
        })
    );
    assert_eq!(
        content[2],
        json!({
            "type": "image_url",
            "image_url": { "url": "https://example.com/cat.png" }
        })
    );
}

/// Text-only messages keep the plain-string fast path (no regression from
/// the block translation).
#[tokio::test]
async fn text_only_message_still_serializes_as_a_plain_string() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(nominal_body()))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    p.chat(
        request("command-r-plus", "salut", false),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["messages"][0]["content"], "salut");
}

/// The M8 pre-flight capability hooks (issue #73): Cohere v2 fetches remote
/// `http(s)` image URLs itself, so `LM-2004` must not fire; the provider-
/// native Anthropic/Gemini file references stay rejected (`LM-2008`).
#[test]
fn vision_capability_hooks_match_cohere_v2() {
    let p = provider("http://unused.invalid".to_owned());
    assert!(p.accepts_remote_image_url());
    assert!(!p.accepts_anthropic_file_id());
    assert!(!p.accepts_gemini_file_uri());
}

/// Cancellation still aborts promptly on the vision path (mirrors the
/// text-only cancellation test above).
#[tokio::test]
async fn cancellation_aborts_an_in_flight_vision_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(nominal_body())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let req = vision_request("command-a-vision-07-2025", false);
    let started = Instant::now();
    let handle = tokio::spawn(async move { p.chat(req, cancel_child).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation should abort promptly"
    );
}

// --------------------------------------------------------------------------
// Streaming: partial-chunk translation
// --------------------------------------------------------------------------

/// A Cohere v2 stream fixture: role, a two-part text delta, then the
/// terminal `message-end` carrying `finish_reason` + usage.
fn stream_sse_body() -> String {
    [
        (
            "message-start",
            json!({ "type": "message-start", "id": "chat_stream_1" }),
        ),
        (
            "content-start",
            json!({
                "type": "content-start", "index": 0,
                "delta": { "message": { "content": { "type": "text", "text": "" } } }
            }),
        ),
        (
            "content-delta",
            json!({
                "type": "content-delta", "index": 0,
                "delta": { "message": { "content": { "text": "Bon" } } }
            }),
        ),
        (
            "content-delta",
            json!({
                "type": "content-delta", "index": 0,
                "delta": { "message": { "content": { "text": "jour" } } }
            }),
        ),
        ("content-end", json!({ "type": "content-end", "index": 0 })),
        (
            "message-end",
            json!({
                "type": "message-end",
                "delta": {
                    "finish_reason": "COMPLETE",
                    "usage": {
                        "billed_units": { "input_tokens": 4, "output_tokens": 2 },
                        "tokens": { "input_tokens": 5, "output_tokens": 3 }
                    }
                }
            }),
        ),
    ]
    .iter()
    .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
    .collect()
}

#[tokio::test]
async fn streaming_translates_partial_chunks_to_openai_shape() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream_sse_body()),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let chunks: Vec<_> = p
        .chat_stream(
            request("command-r-plus", "salut", true),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    let chunks: Vec<_> = chunks.into_iter().map(|c| c.unwrap()).collect();

    // role chunk, 2 text deltas, finish chunk (content-start/-end contribute
    // nothing - see the module docs).
    assert_eq!(chunks.len(), 4, "{chunks:#?}");
    assert_eq!(chunks[0].id, "chat_stream_1");
    assert_eq!(
        chunks[0].choices[0].delta.role.as_deref(),
        Some("assistant")
    );
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("Bon"));
    assert_eq!(chunks[2].choices[0].delta.content.as_deref(), Some("jour"));
    assert_eq!(chunks[3].choices[0].finish_reason.as_deref(), Some("stop"));
    let usage = chunks[3].usage.unwrap();
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 3);

    // Streaming asked upstream with `stream: true`.
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn streaming_bytes_end_with_done_only_on_a_genuine_terminator() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream_sse_body()),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let bytes: Vec<_> = p
        .chat_stream_bytes(
            request("command-r-plus", "salut", true),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    let text: String = bytes
        .into_iter()
        .map(|b| String::from_utf8(b.unwrap().to_vec()).unwrap())
        .collect();

    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    assert_eq!(text.matches("chat.completion.chunk").count(), 4);
}

#[tokio::test]
async fn truncated_stream_never_fabricates_a_done_terminator() {
    // The upstream dies right before the terminal `message-end` event.
    let full = stream_sse_body();
    let cut = full.find("event: message-end").unwrap();
    let truncated = full[..cut].to_owned();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(truncated),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let bytes: Vec<_> = p
        .chat_stream_bytes(
            request("command-r-plus", "salut", true),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    let text: String = bytes
        .into_iter()
        .map(|b| String::from_utf8(b.unwrap().to_vec()).unwrap())
        .collect();

    assert!(!text.contains("data: [DONE]"), "got: {text}");
}

// --------------------------------------------------------------------------
// Streaming: client disconnection aborts the upstream connection
// --------------------------------------------------------------------------

/// A raw TCP "upstream" that streams SSE frames and signals when its
/// connection is closed by the peer (the provider dropping its response
/// stream), mirroring the server-level abort-detection test.
async fn spawn_abort_detecting_upstream() -> (String, tokio::sync::oneshot::Receiver<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut req = [0u8; 4096];
        let _ = socket.read(&mut req).await;
        let (mut rd, mut wr) = socket.split();
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        if wr.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let frame = "event: content-delta\ndata: {\"type\":\"content-delta\",\"index\":0,\"delta\":{\"message\":{\"content\":{\"text\":\"x\"}}}}\n\n";

        let write_loop = async {
            loop {
                if wr.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let read_eof = async {
            let mut buf = [0u8; 256];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        };
        tokio::select! {
            () = write_loop => {},
            () = read_eof => {},
        }
        let _ = tx.send(());
    });

    (format!("http://{addr}"), rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_stream_aborts_the_upstream_connection() {
    let (upstream_url, rx) = spawn_abort_detecting_upstream().await;
    let p = CohereProvider::new(
        reqwest::Client::new(),
        "cohere-test",
        Some(upstream_url),
        Some(DUMMY_KEY.to_owned()),
    );

    let mut stream = p
        .chat_stream(
            request("command-r-plus", "hi", true),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Consume the first translated chunk, then disconnect by dropping the
    // stream (the client-disconnect scenario the server exercises via the
    // response body drop).
    let first = stream.next().await;
    assert!(first.is_some(), "expected at least one streamed chunk");
    drop(stream);

    let closed = tokio::time::timeout(Duration::from_secs(3), rx).await;
    assert!(
        matches!(closed, Ok(Ok(()))),
        "upstream connection was not aborted after the stream was dropped"
    );
}
