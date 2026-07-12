//! End-to-end HTTP tests for `POST /v1/chat/completions`: non-streaming routing
//! and passthrough, Anthropic translation, streaming SSE (chunks + `[DONE]`),
//! client-disconnect cancellation, and routing/validation errors.

// Building a fixture SSE body is clearest with format!; the style lints don't
// earn their keep in test scaffolding.
#![allow(clippy::format_collect)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use ferrogate_core::Capability;
use ferrogate_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// One OpenAI-kind provider (chat + an embed-only model) pointed at `upstream`.
fn openai_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![
            ModelSpec {
                id: "gpt".to_owned(),
                upstream_id: "gpt-4o-2024-08-06".to_owned(),
                capabilities: vec![Capability::Chat],
            },
            ModelSpec {
                id: "embed-only".to_owned(),
                upstream_id: "text-embedding-3-small".to_owned(),
                capabilities: vec![Capability::Embed],
            },
        ],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

fn anthropic_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "anthropic".to_owned(),
        kind: ProviderKind::Anthropic,
        api_key: Some("sk-ant-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![ModelSpec {
            id: "claude".to_owned(),
            upstream_id: "claude-3-5-sonnet".to_owned(),
            capabilities: vec![Capability::Chat],
        }],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

fn openai_chat_body(model: &str) -> Value {
    json!({
        "object": "chat.completion",
        "id": "chatcmpl-1",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hi there" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10 }
    })
}

#[tokio::test]
async fn non_streaming_happy_path_resolves_alias_and_returns_openai_shape() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_chat_body("gpt-4o-2024-08-06")),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hi there");
    assert_eq!(body["usage"]["total_tokens"], 10);

    // The client-facing id was resolved to the upstream id, stream forced false.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-4o-2024-08-06");
    assert_eq!(sent["stream"], false);
}

#[tokio::test]
async fn unknown_model_is_404_fg2001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "nope", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "FG-2001");
}

#[tokio::test]
async fn embed_only_model_requested_for_chat_is_400_fg2002() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "embed-only", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "FG-2002");
}

#[tokio::test]
async fn empty_messages_is_400_fg1001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "FG-1001");
}

#[tokio::test]
async fn upstream_5xx_propagates_as_502_fg3003() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["code"],
        "FG-3003"
    );
}

#[tokio::test]
async fn anthropic_translation_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet",
            "content": [{ "type": "text", "text": "bonjour" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 12, "output_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(anthropic_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "salut" }
            ],
            "max_tokens": 100
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Anthropic response translated back to OpenAI shape.
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "bonjour");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 12);
    assert_eq!(body["usage"]["completion_tokens"], 3);

    // Outgoing Anthropic request: system hoisted, only the user message remains.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "claude-3-5-sonnet");
    assert_eq!(sent["system"], "be terse");
    assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["max_tokens"], 100);
}

/// Build an upstream OpenAI-style SSE body of `n` chunk frames + `[DONE]`, with
/// deliberately compact, quirky spacing so a byte-identical assertion proves the
/// gateway forwarded verbatim rather than deserializing and re-serializing.
fn upstream_sse_body(n: usize) -> String {
    let frames: String = (0..n)
        .map(|i| {
            format!(
                "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
            )
        })
        .collect();
    format!("{frames}data: [DONE]\n\n")
}

#[tokio::test]
async fn streaming_passthrough_is_byte_identical() {
    let upstream = MockServer::start().await;
    let body = upstream_sse_body(100);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.clone()),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");

    // Zero-copy passthrough: the client receives the upstream body byte-for-byte
    // (all 100 chunks + [DONE]); any re-serialization would perturb formatting.
    let text = resp.text().await.unwrap();
    assert_eq!(text, body);
    assert_eq!(text.matches("chat.completion.chunk").count(), 100);
    assert!(text.ends_with("data: [DONE]\n\n"));

    // The gateway asked the upstream to stream with usage (ADR 003 hook).
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn streaming_client_disconnect_does_not_hang_server() {
    let upstream = MockServer::start().await;
    // Upstream is slow to even produce the response; client cuts well before.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse_body(3))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    // Client gives up after 200ms, well before the 3s upstream delay.
    let started = std::time::Instant::now();
    let result = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_millis(200))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "x" }],
            "stream": true
        }))
        .send()
        .await;
    assert!(result.is_err(), "client should have timed out");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "client aborted promptly, not after the 3s upstream delay"
    );

    // Server stays responsive afterwards (the cancelled request didn't wedge it).
    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}

/// A raw TCP "upstream" that streams SSE frames and signals when its connection
/// is closed by the peer (the gateway). Detects the FIN via a 0-byte read.
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
        // Drain the request (small, one read on loopback) before responding, so
        // we don't close/respond while the gateway is still writing its request.
        let mut req = [0u8; 4096];
        let _ = socket.read(&mut req).await;
        let (mut rd, mut wr) = socket.split();
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        if wr.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let frame = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n";

        // Keep streaming frames while watching for the peer's FIN. When the
        // gateway drops its upstream connection (because the client
        // disconnected), the read side sees EOF and we signal.
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
                    Ok(0) | Err(_) => break, // peer closed
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
async fn streaming_client_disconnect_aborts_upstream_connection() {
    // Acceptance criterion 2: a client disconnect mid-stream must close the
    // upstream connection promptly (the LiteLLM #22805 lesson). This exercises
    // the drop-guard / bytes_stream-drop path that the timing-only test cannot.
    let (upstream_url, rx) = spawn_abort_detecting_upstream().await;
    let base = common::spawn_with(openai_registry(&upstream_url), LIMIT).await;

    let client = reqwest::Client::new();
    let mut resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "x" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Read the first streamed frame, then disconnect by dropping the response.
    let first = resp.chunk().await.unwrap();
    assert!(first.is_some(), "expected at least one streamed frame");
    drop(resp);

    // The upstream observes its connection torn down within the window.
    let closed = tokio::time::timeout(Duration::from_secs(3), rx).await;
    assert!(
        matches!(closed, Ok(Ok(()))),
        "upstream connection was not aborted after client disconnect"
    );
}
