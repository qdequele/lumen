//! End-to-end HTTP tests for `POST /v1/chat/completions`: non-streaming routing
//! and passthrough, Anthropic translation, streaming SSE (chunks + `[DONE]`),
//! client-disconnect cancellation, and routing/validation errors.

// Building a fixture SSE body is clearest with format!; the style lints don't
// earn their keep in test scaffolding.
#![allow(clippy::format_collect)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
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
                modalities: Vec::new(),
            },
            ModelSpec {
                id: "embed-only".to_owned(),
                upstream_id: "text-embedding-3-small".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: Vec::new(),
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
            modalities: Vec::new(),
        }],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

fn google_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "google".to_owned(),
        kind: ProviderKind::Google,
        api_key: Some("goog-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![ModelSpec {
            id: "gemini".to_owned(),
            upstream_id: "gemini-2.0-flash".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: Vec::new(),
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
async fn openai_compatible_kind_routes_through_the_openai_path() {
    // A new OpenAI-compatible kind (Groq) pointed at a mock via base_url must
    // route exactly like the OpenAI kind — proving the shared provider wiring.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_body("llama-3.3-70b")))
        .mount(&upstream)
        .await;

    let specs = vec![ProviderSpec {
        name: "groq".to_owned(),
        kind: ProviderKind::Groq,
        api_key: Some("gsk-test".to_owned()),
        base_url: Some(upstream.uri()), // override the built-in api.groq.com default
        models: vec![ModelSpec {
            id: "fast".to_owned(),
            upstream_id: "llama-3.3-70b".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: Vec::new(),
        }],
    }];
    let registry = Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"));
    let base = common::spawn_with(registry, LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fast", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    // Alias resolved to the upstream id, just like the OpenAI kind.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "llama-3.3-70b");
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
    assert_eq!(body["error"]["code"], "LM-2001");
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
    assert_eq!(body["error"]["code"], "LM-2002");
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
    assert_eq!(body["error"]["code"], "LM-1001");
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
        "LM-3003"
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

#[tokio::test]
async fn google_gemini_translation_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "salut" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [
                { "role": "system", "content": "sois bref" },
                { "role": "user", "content": "bonjour" }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "salut");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 7);

    // Outgoing Gemini request: system hoisted, roles mapped, model in the path.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["systemInstruction"]["parts"][0]["text"], "sois bref");
    assert_eq!(sent["contents"][0]["role"], "user");
    assert_eq!(sent["contents"][0]["parts"][0]["text"], "bonjour");
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

// ---- M4 finish: streaming translation + stream guards ----------------------

/// Anthropic SSE event fixture (text + tool_use), in upstream wire format.
fn anthropic_sse_body() -> String {
    [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_str", "model": "claude-3-5-sonnet",
                    "usage": { "input_tokens": 12 }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "Hel" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "lo" }
            }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 1,
                "content_block": { "type": "tool_use", "id": "toolu_9", "name": "lookup" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"x\"}" }
            }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 9 }
            }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]
    .iter()
    .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
    .collect()
}

/// Parse the `data:` payloads of an SSE body (excluding `[DONE]` and comments).
fn sse_data_frames(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("frame is JSON"))
        .collect()
}

#[tokio::test]
async fn anthropic_streaming_translates_events_to_openai_chunks() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(anthropic_sse_body()),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(anthropic_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Clean upstream termination: translated [DONE], and no LM-3010.
    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    assert!(!text.contains("LM-3010"));

    let chunks = sse_data_frames(&text);
    // role, 2 text deltas, tool open, tool args, finish.
    assert_eq!(chunks.len(), 6, "got: {text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[0]["id"], "msg_str");
    assert_eq!(chunks[0]["model"], "claude-3-5-sonnet");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hel");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "lo");
    let tool_open = &chunks[3]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tool_open["index"], 0);
    assert_eq!(tool_open["id"], "toolu_9");
    assert_eq!(tool_open["function"]["name"], "lookup");
    let tool_args = &chunks[4]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tool_args["function"]["arguments"], "{\"q\":\"x\"}");
    assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
    // ADR 003: the final chunk carries full usage.
    assert_eq!(chunks[5]["usage"]["prompt_tokens"], 12);
    assert_eq!(chunks[5]["usage"]["completion_tokens"], 9);
    assert_eq!(chunks[5]["usage"]["total_tokens"], 21);

    // The upstream was asked to stream.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn gemini_streaming_translates_fragments_to_openai_chunks() {
    let upstream = MockServer::start().await;
    let body = [
        json!({
            "candidates": [
                { "content": { "parts": [{ "text": "Bon" }], "role": "model" } }
            ]
        }),
        json!({
            "candidates": [{
                "content": { "parts": [{ "text": "jour" }], "role": "model" },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8
            }
        }),
    ]
    .iter()
    .map(|data| format!("data: {data}\n\n"))
    .collect::<String>();

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{ "role": "user", "content": "salut" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    assert!(!text.contains("LM-3010"));

    let chunks = sse_data_frames(&text);
    // role, 2 text deltas, finish.
    assert_eq!(chunks.len(), 4, "got: {text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Bon");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "jour");
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[3]["usage"]["total_tokens"], 8);
}

#[tokio::test]
async fn upstream_stream_without_done_yields_fg3010_error_frame() {
    let upstream = MockServer::start().await;
    // Two valid chunks, then the body just ends — no `data: [DONE]`.
    let truncated: String = upstream_sse_body(2)
        .strip_suffix("data: [DONE]\n\n")
        .unwrap()
        .to_owned();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(truncated),
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
    let text = resp.text().await.unwrap();

    // Both real chunks were forwarded, then a terminal LM-3010 error frame
    // (criterion 5) — and the stream ended cleanly, no hang.
    assert_eq!(text.matches("chat.completion.chunk").count(), 2);
    assert!(!text.contains("data: [DONE]"));
    let frames = sse_data_frames(&text);
    let last = frames.last().unwrap();
    assert_eq!(last["error"]["code"], "LM-3010");
    assert_eq!(last["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn first_token_timeout_non_streaming_is_504_fg3011() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_chat_body("gpt-4o-2024-08-06"))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&upstream)
        .await;

    let guards = lumen_server::StreamGuards {
        first_token_timeout: Duration::from_millis(150),
        heartbeat_interval: Duration::from_secs(15),
    };
    let base = common::spawn_with_guards(openai_registry(&upstream.uri()), LIMIT, guards).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 504);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3011");
    assert_eq!(body["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn first_token_timeout_streaming_before_headers_is_504_fg3011() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse_body(1))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&upstream)
        .await;

    let guards = lumen_server::StreamGuards {
        first_token_timeout: Duration::from_millis(150),
        heartbeat_interval: Duration::from_secs(15),
    };
    let base = common::spawn_with_guards(openai_registry(&upstream.uri()), LIMIT, guards).await;

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

    // The upstream never answered within the window: the stream never started,
    // so this is an honest 504 JSON envelope rather than an SSE error frame.
    assert_eq!(resp.status(), 504);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3011");
}
