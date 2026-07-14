//! End-to-end latency observability tests: every endpoint feeds the
//! `lumen_http_request_duration_seconds` histogram (labelled by matched route,
//! never the raw path), and accounted API calls additionally feed
//! `lumen_request_duration_seconds` with capability/model/provider labels.

mod common;

use std::sync::Arc;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// One OpenAI-kind chat provider pointed at `upstream`.
fn openai_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        models: vec![ModelSpec {
            id: "gpt".to_owned(),
            upstream_id: "gpt-4o-2024-08-06".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    Arc::new(Registry::build(specs, http::build_client()).expect("registry builds"))
}

async fn scrape_metrics(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape /metrics")
        .text()
        .await
        .expect("metrics body")
}

#[tokio::test]
async fn every_endpoint_feeds_the_http_latency_histogram() {
    let base = common::spawn().await;

    let health = reqwest::get(format!("{base}/health"))
        .await
        .expect("GET /health");
    assert_eq!(health.status(), 200);

    let out = scrape_metrics(&base).await;
    assert!(
        out.contains("lumen_http_request_duration_seconds"),
        "histogram missing from /metrics:\n{out}"
    );
    assert!(
        out.contains(r#"path="/health""#),
        "matched route label missing:\n{out}"
    );
    assert!(out.contains(r#"method="GET""#));
    assert!(out.contains(r#"status="200""#));
}

#[tokio::test]
async fn the_metrics_endpoint_itself_is_measured() {
    let base = common::spawn().await;

    // First scrape observes itself only on the NEXT scrape.
    let _ = scrape_metrics(&base).await;
    let out = scrape_metrics(&base).await;
    assert!(out.contains(r#"path="/metrics""#), "{out}");
}

#[tokio::test]
async fn unmatched_routes_get_a_bounded_label_not_the_raw_path() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/nope/secret-looking-path"))
        .await
        .expect("GET unmatched");
    assert_eq!(resp.status(), 404);

    let out = scrape_metrics(&base).await;
    // The raw path must NEVER become a label (cardinality + privacy).
    assert!(!out.contains("secret-looking-path"), "{out}");
    assert!(out.contains(r#"path="unmatched""#), "{out}");
    assert!(out.contains(r#"status="404""#), "{out}");
}

#[tokio::test]
async fn nonstandard_methods_get_a_bounded_label_not_the_raw_string() {
    let base = common::spawn().await;

    let method = reqwest::Method::from_bytes(b"QWERTY").expect("custom method");
    let resp = reqwest::Client::new()
        .request(method, format!("{base}/health"))
        .send()
        .await
        .expect("QWERTY /health");
    assert_eq!(resp.status(), 405);

    let out = scrape_metrics(&base).await;
    // An arbitrary extension method must never mint a new label value.
    assert!(!out.contains("QWERTY"), "{out}");
    assert!(out.contains(r#"method="other""#), "{out}");
}

#[tokio::test]
async fn chat_requests_feed_the_per_model_latency_histogram() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions");
    assert_eq!(resp.status(), 200);

    let out = scrape_metrics(&base).await;
    assert!(
        out.contains("lumen_request_duration_seconds"),
        "per-capability histogram missing:\n{out}"
    );
    let line = out
        .lines()
        .find(|l| l.starts_with("lumen_request_duration_seconds_count") && l.contains("chat"))
        .unwrap_or_else(|| panic!("no chat duration sample:\n{out}"));
    assert!(line.contains(r#"capability="chat""#), "{line}");
    assert!(line.contains(r#"model="gpt""#), "{line}");
    assert!(line.contains(r#"provider="openai""#), "{line}");
    assert!(line.contains(r#"status="200""#), "{line}");
    // The HTTP-level histogram sees the same request under its route.
    assert!(out.contains(r#"path="/v1/chat/completions""#), "{out}");
}
