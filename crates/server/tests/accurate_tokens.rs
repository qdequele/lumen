//! End-to-end tests for the opt-in accurate tokenizer (ADR 003, issue #8):
//! with `[tokenizer] mode = "accurate"` and a usage-silent upstream, the
//! response envelope must carry the INLINE HEURISTIC estimate (proving the
//! request path never waits on a BPE pass), while the accurate BPE count
//! lands afterwards on the Prometheus token counters via the deferred
//! background accounting close.
//!
//! The fixture string "hello world" separates the two estimators cleanly:
//! heuristic = ceil(11 bytes / 4) = 3 tokens; exact BPE (cl100k_base and
//! o200k_base alike) = 2 tokens.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::config::{TokenizerConfig, TokenizerMode};
use lumen_server::tokenizer::TokenCounter;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// Spawn the app with the ACCURATE tokenizer attached.
async fn spawn_accurate(registry: Arc<Registry>) -> String {
    let counter = Arc::new(TokenCounter::from_config(&TokenizerConfig {
        mode: TokenizerMode::Accurate,
    }));
    assert!(counter.is_accurate(), "accurate encoders must load");
    let state = common::base_state(registry).with_token_counter(counter);
    common::spawn_state(state, LIMIT).await
}

/// Read `lumen_tokens_total{capability=...,direction=...}` from `/metrics`.
async fn token_metric(base: &str, capability: &str, direction: &str) -> Option<f64> {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    text.lines()
        .find(|line| {
            line.starts_with("lumen_tokens_total{")
                && line.contains(&format!("capability=\"{capability}\""))
                && line.contains(&format!("direction=\"{direction}\""))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

/// Poll `/metrics` until the counter reaches `expected` (the accounting close
/// is deferred to a background refinement task, so it lands asynchronously).
async fn wait_for_tokens(base: &str, capability: &str, direction: &str, expected: f64) {
    let mut last = None;
    for _ in 0..250 {
        last = token_metric(base, capability, direction).await;
        if last == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "lumen_tokens_total{{capability={capability},direction={direction}}} \
         never reached {expected} (last seen: {last:?})"
    );
}

#[tokio::test]
async fn chat_envelope_stays_heuristic_while_metrics_get_the_accurate_count() {
    let upstream = MockServer::start().await;
    // Usage-silent upstream: no `usage` object at all -> tier-2 estimation.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4-0613",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello world" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&upstream)
        .await;

    // Client-facing id "gpt-4" carries the cl100k_base family prefix.
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test".to_owned()),
        base_url: Some(upstream.uri()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gpt-4".to_owned(),
            upstream_id: "gpt-4-0613".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    let registry = Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let base = spawn_accurate(registry).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4",
            "messages": [{ "role": "user", "content": "hello world" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // The envelope is the INLINE HEURISTIC, flagged estimated - never the BPE
    // count. Heuristic: input ceil(11/4)=3 + 4 overhead = 7; output 3.
    // (Accurate would be 6 and 2 - the request path must not have waited.)
    assert_eq!(body["usage"]["estimated"], true);
    assert_eq!(body["usage"]["prompt_tokens"], 7);
    assert_eq!(body["usage"]["completion_tokens"], 3);

    // The accurate BPE count (input 2+4=6, output 2) lands on the metrics
    // surface via the deferred background close.
    wait_for_tokens(&base, "chat", "input", 6.0).await;
    wait_for_tokens(&base, "chat", "output", 2.0).await;
}

#[tokio::test]
async fn embed_envelope_stays_heuristic_while_metrics_get_the_accurate_count() {
    let upstream = MockServer::start().await;
    // Usage-silent embeddings upstream (the TEI-shaped gap ADR 003 closes).
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1] },
                { "object": "embedding", "index": 1, "embedding": [0.2] }
            ],
            "model": "text-embedding-3-small"
        })))
        .mount(&upstream)
        .await;

    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test".to_owned()),
        base_url: Some(upstream.uri()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "text-embedding-3-small".to_owned(),
            upstream_id: "text-embedding-3-small".to_owned(),
            capabilities: vec![Capability::Embed],
            modalities: vec!["text".to_owned()],
        }],
    }];
    let registry = Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let base = spawn_accurate(registry).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({
            "model": "text-embedding-3-small",
            "input": ["hello world", "hello world"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // Envelope: heuristic 3+3=6, flagged. (Accurate would be 4.)
    assert_eq!(body["usage"]["estimated"], true);
    assert_eq!(body["usage"]["prompt_tokens"], 6);

    // Metrics: the accurate BPE batch count, 2+2=4.
    wait_for_tokens(&base, "embed", "input", 4.0).await;
}

#[tokio::test]
async fn rerank_metrics_get_the_accurate_count_via_the_deferred_close() {
    let upstream = MockServer::start().await;
    // No billed_units: search units are gateway-derived; tokens are always
    // gateway-estimated for rerank regardless.
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 0, "relevance_score": 0.5 }]
        })))
        .mount(&upstream)
        .await;

    // Contrived alias carrying an OpenAI family prefix, to exercise the
    // deferred rerank refinement wiring (real rerank ids never match one and
    // keep the heuristic).
    let specs = vec![ProviderSpec {
        name: "cohere".to_owned(),
        kind: ProviderKind::Cohere,
        api_key: Some("sk-test".to_owned()),
        base_url: Some(upstream.uri()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gpt-4o-rerank".to_owned(),
            upstream_id: "rerank-v3.5".to_owned(),
            capabilities: vec![Capability::Rerank],
            modalities: vec!["text".to_owned()],
        }],
    }];
    let registry = Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let base = spawn_accurate(registry).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "gpt-4o-rerank",
            "query": "hello world",
            "documents": ["hello world"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // The envelope carries (estimated) search units, never tokens.
    assert_eq!(body["usage"]["search_units"], 1);
    assert_eq!(body["usage"]["estimated"], true);

    // Metrics: accurate query(2) x 1 document + document(2) = 4 tokens.
    // (Heuristic would be 3 + 3 = 6.)
    wait_for_tokens(&base, "rerank", "input", 4.0).await;
}
