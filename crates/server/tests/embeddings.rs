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
        strict: false,
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
            ModelSpec {
                id: "embed-image".to_owned(),
                upstream_id: "multimodal-embed".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: vec!["text".to_owned(), "image".to_owned()],
            },
        ],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

/// Registry with one Cohere-kind provider pointed at `upstream`, exposing a
/// single embedding model (issue #22 `input_type` override tests).
fn registry_for_cohere(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "cohere".to_owned(),
        kind: ProviderKind::Cohere,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        strict: false,
        models: vec![ModelSpec {
            id: "embed-multilingual".to_owned(),
            upstream_id: "embed-v4.0".to_owned(),
            capabilities: vec![Capability::Embed],
            modalities: vec!["text".to_owned()],
        }],
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
async fn base64_encoding_format_re_encodes_vectors_on_the_way_out() {
    let upstream = MockServer::start().await;
    // Upstream returns a plain float array; the gateway must re-encode it as
    // base64 because the CLIENT asked for encoding_format: "base64" (issue #25).
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [1.0, 2.0] }],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-small",
            "input": "hello",
            "encoding_format": "base64"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // The vector is now a base64 string, not a float array.
    let b64 = body["data"][0]["embedding"]
        .as_str()
        .expect("base64 string embedding");
    let expected = lumen_core::encode_embedding_base64(&[1.0, 2.0]);
    assert_eq!(b64, expected);
}

#[tokio::test]
async fn token_input_to_text_only_provider_is_400_fg1001_without_upstream_call() {
    // A TEI-kind provider cannot consume pre-tokenized input; the gateway must
    // return an honest 400 (LM-1001) and never contact the upstream (issue #25
    // review). No mock is mounted: any upstream call would still show up in
    // `received_requests`.
    let upstream = MockServer::start().await;
    let specs = vec![ProviderSpec {
        name: "tei".to_owned(),
        kind: ProviderKind::Tei,
        api_key: None,
        base_url: Some(upstream.uri()),
        strict: false,
        models: vec![ModelSpec {
            id: "tei-embed".to_owned(),
            upstream_id: "tei-embed".to_owned(),
            capabilities: vec![Capability::Embed],
            modalities: vec!["text".to_owned()],
        }],
    }];
    let registry = Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"));
    let base = common::spawn_with(registry, LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "tei-embed", "input": [[1, 2], [3, 4]] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("pre-tokenized"),
        "message names the input shape: {body}"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
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
async fn image_input_to_text_only_model_is_400_fg2003_without_upstream_call() {
    let upstream = MockServer::start().await;
    // No mock mounted: if the handler calls upstream, the request 404s there and
    // this test's assertions on `received_requests` catch the leak.
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-small",
            "input": [[
                {"type": "text", "text": "a caption"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2003");
    assert_eq!(body["error"]["type"], "invalid_request");

    // Fail-fast: the upstream must never have been contacted.
    let requests = upstream.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "no upstream call for a rejected image request"
    );
}

#[tokio::test]
async fn data_uri_image_is_counted_in_media_metrics() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "multimodal-embed",
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    // A 3-byte image inline as a data: URI (no fetch needed).
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-image",
            "input": [[
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The media counters must now show one image of 3 decoded bytes.
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("lumen_media_total"),
        "media count metric present"
    );
    assert!(
        metrics.contains("lumen_media_bytes_total"),
        "media bytes metric present"
    );
    assert!(metrics.contains(r#"media_type="image""#));
}

#[tokio::test]
async fn remote_image_url_with_fetch_disabled_is_400_fg2005() {
    let upstream = MockServer::start().await;
    // Default test state has image fetching disabled, so a remote image URL to
    // an image-capable model is rejected with LM-2005 before any upstream call.
    let base = common::spawn_with(registry_for(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-image",
            "input": [[
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}
            ]]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2005");
    assert!(upstream.received_requests().await.unwrap().is_empty());
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
async fn unknown_input_type_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(registry_for_cohere(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-multilingual",
            "input": "hello",
            "input_type": "not_a_real_type"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");

    // Rejected before any upstream call.
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn input_type_override_reaches_cohere_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1, 0.2]] },
            "meta": { "billed_units": { "input_tokens": 2 } }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(registry_for_cohere(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "embed-multilingual",
            "input": "find me the best result",
            "input_type": "search_query"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["input_type"], "search_query");
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
