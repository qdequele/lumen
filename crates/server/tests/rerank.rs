//! End-to-end HTTP tests for `POST /v1/rerank`: Cohere-format routing, alias
//! resolution, gateway-side ordering / `top_n` clamp / document echo, upstream
//! error propagation, and the LM-2010 empty-documents rejection. The upstream
//! is a wiremock server speaking Cohere's v2 rerank schema.

mod common;

use std::sync::Arc;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Registry with one Cohere-kind provider pointed at `upstream`, exposing a
/// rerank model (aliased) and an embed-only model.
fn registry_for(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "cohere".to_owned(),
        kind: ProviderKind::Cohere,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![
            ModelSpec {
                id: "rerank-fast".to_owned(),
                upstream_id: "rerank-v3.5".to_owned(),
                capabilities: vec![Capability::Rerank],
            },
            ModelSpec {
                id: "embed-only".to_owned(),
                upstream_id: "embed-v4.0".to_owned(),
                capabilities: vec![Capability::Embed],
            },
        ],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

const LIMIT: usize = 10 * 1024 * 1024;

/// Mount a Cohere v2 rerank responder returning scores deliberately OUT of
/// order, so the gateway's descending sort is what produces the final order.
async fn mount_cohere_rerank(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 0, "relevance_score": 0.10 },
                { "index": 1, "relevance_score": 0.99 },
                { "index": 2, "relevance_score": 0.50 }
            ],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn happy_path_sorts_by_score_and_resolves_alias() {
    let upstream = MockServer::start().await;
    mount_cohere_rerank(&upstream).await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "rerank-fast",
            "query": "best fruit",
            "documents": ["apple", "banana", "cherry"]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // Sorted by descending score; index points to the ORIGINAL position.
    assert_eq!(body["results"][0]["index"], 1);
    assert_eq!(body["results"][1]["index"], 2);
    assert_eq!(body["results"][2]["index"], 0);
    assert_eq!(body["results"][0]["relevance_score"], 0.99);
    assert_eq!(body["usage"]["search_units"], 1);
    // return_documents defaulted to false → no echoed document.
    assert!(body["results"][0].get("document").is_none());

    // The alias was resolved to the upstream id before the call.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "rerank-v3.5");
}

#[tokio::test]
async fn return_documents_true_echoes_source_text_by_original_index() {
    let upstream = MockServer::start().await;
    mount_cohere_rerank(&upstream).await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "rerank-fast",
            "query": "q",
            "documents": ["apple", "banana", "cherry"],
            "return_documents": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Top result is original index 1 → "banana".
    assert_eq!(body["results"][0]["index"], 1);
    assert_eq!(body["results"][0]["document"]["text"], "banana");
    assert_eq!(body["results"][2]["document"]["text"], "apple");
}

#[tokio::test]
async fn top_n_greater_than_documents_is_clamped_and_forwarded() {
    let upstream = MockServer::start().await;
    mount_cohere_rerank(&upstream).await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "rerank-fast",
            "query": "q",
            "documents": ["a", "b", "c"],
            "top_n": 99
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    // The upstream saw the clamped value (3), never 99.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["top_n"], 3);
}

#[tokio::test]
async fn documents_accept_object_form() {
    let upstream = MockServer::start().await;
    mount_cohere_rerank(&upstream).await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "rerank-fast",
            "query": "q",
            "documents": [{ "text": "apple" }, "banana", { "text": "cherry" }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    // The provider received plain text strings for all three documents.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["documents"][0], "apple");
    assert_eq!(sent["documents"][2], "cherry");
}

#[tokio::test]
async fn empty_documents_is_400_fg2010() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rerank-fast", "query": "q", "documents": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2010");
    assert_eq!(body["error"]["type"], "invalid_request");
    // No upstream call was made for a request rejected at the edge.
    assert_eq!(upstream.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn unknown_model_is_404_fg2001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "nope", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2001");
}

#[tokio::test]
async fn embed_only_model_requested_for_rerank_is_400_fg2002() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "embed-only", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2002");
}

#[tokio::test]
async fn upstream_5xx_propagates_as_502_fg3003() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rerank-fast", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3003");
    assert_eq!(body["error"]["type"], "upstream_error");
}
