//! End-to-end HTTP tests for `POST /v1/chat/completions`: non-streaming routing
//! and passthrough, Anthropic translation, streaming SSE (chunks + `[DONE]`),
//! client-disconnect cancellation, and routing/validation errors.

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

#[tokio::test]
async fn streaming_yields_sse_chunks_then_done() {
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

    let text = resp.text().await.unwrap();
    // At least one data frame carrying a chunk, and a terminal [DONE].
    assert!(text.contains("chat.completion.chunk"), "body: {text}");
    assert!(text.contains("hi there"), "body: {text}");
    assert!(text.contains("data: [DONE]"), "body: {text}");
}

#[tokio::test]
async fn streaming_client_disconnect_does_not_hang_server() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_chat_body("gpt-4o-2024-08-06"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    // Client gives up after 200ms, well before the 3s upstream delay.
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

    // Server stays responsive afterwards.
    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}
