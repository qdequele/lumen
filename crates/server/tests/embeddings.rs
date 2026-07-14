//! End-to-end HTTP tests for `POST /v1/embeddings`: routing, alias resolution,
//! upstream error propagation, and client-disconnect handling. The upstream is
//! a wiremock server; LUMEN sits in front of it.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Registry with one OpenAI-kind provider pointed at `upstream`, exposing an
/// embedding model (aliased) and a chat-only model.
fn registry_for(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![
            ModelSpec {
                id: "embed-small".to_owned(),
                upstream_id: "text-embedding-3-small".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: vec!["text".to_owned()],
            },
            ModelSpec {
                id: "chat-only".to_owned(),
                upstream_id: "gpt-4o".to_owned(),
                capabilities: vec![Capability::Chat],
                modalities: vec!["text".to_owned()],
            },
        ],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

const LIMIT: usize = 10 * 1024 * 1024;

#[tokio::test]
async fn happy_path_returns_openai_format_and_resolves_alias() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "embed-small", "input": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["embedding"][0], 0.1);

    // The client-facing id "embed-small" was resolved to the upstream id.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "text-embedding-3-small");
}

#[tokio::test]
async fn unknown_model_is_404_fg2001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "does-not-exist", "input": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2001");
    assert_eq!(body["error"]["type"], "invalid_request");
}

#[tokio::test]
async fn chat_only_model_requested_for_embedding_is_400_fg2002() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "chat-only", "input": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2002");
}

#[tokio::test]
async fn upstream_429_propagates_as_429_fg3001_with_retry_after() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "embed-small", "input": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3001");
    assert_eq!(body["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn malformed_upstream_response_is_502_fg3002_never_500() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "embed-small", "input": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3002");
}

#[tokio::test]
async fn empty_input_is_400_fg1001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "embed-small", "input": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");
}

#[tokio::test]
async fn client_disconnect_during_slow_upstream_does_not_hang_server() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "object": "list",
                    "data": [{ "object": "embedding", "index": 0, "embedding": [0.0] }],
                    "model": "text-embedding-3-small",
                    "usage": { "prompt_tokens": 1, "total_tokens": 1 }
                }))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    // Client gives up after 200ms - well before the 3s upstream delay. This
    // drops the connection, which drops the handler future and cancels the
    // upstream call.
    let result = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .timeout(Duration::from_millis(200))
        .json(&json!({ "model": "embed-small", "input": "x" }))
        .send()
        .await;
    assert!(result.is_err(), "client should have timed out");

    // The request did reach the upstream (the gateway forwarded it)...
    let calls = upstream.received_requests().await.unwrap().len();
    assert_eq!(calls, 1);

    // ...and the server is still responsive afterwards (didn't hang/crash).
    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}
