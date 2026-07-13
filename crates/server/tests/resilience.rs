//! M6 acceptance tests: retries, fallback, circuit breaking, streaming
//! non-retry, load isolation and the `Retry-After` floor.
//!
//! These drive the real HTTP stack against wiremock upstreams in real time
//! (small backoffs keep them fast); the precise simulated-time assertions for
//! backoff and `Retry-After` live in the router's unit tests (`retry`), which
//! use `tokio::time` pause.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use figment::{
    providers::{Format, Toml},
    Figment,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

fn config_from(toml: &str) -> Config {
    Figment::new()
        .merge(Toml::string(toml))
        .extract::<Config>()
        .expect("valid test config")
}

/// Spawn the full app from a config, building the registry from the same
/// providers so routing and resilience agree. Returns the base URL.
async fn spawn(config: &Config) -> String {
    let registry = Arc::new(
        Registry::build(config.provider_specs(), http::build_client()).expect("registry builds"),
    );
    let state = common::base_state(registry)
        .with_pricing(CostTable::from_config(config))
        .with_resilience(Arc::new(ResilienceRuntime::from_config(config, None)));
    common::spawn_state(state, LIMIT).await
}

fn chat_body(model: &str) -> Value {
    json!({
        "object": "chat.completion",
        "id": "chatcmpl-1",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
    })
}

async fn post_chat(base: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("request sent")
}

/// A single primary provider, small backoffs. `{url}` is the upstream.
fn single_provider_config(url: &str) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = 3
        retry_base_ms = 20
        retry_max_ms = 100

        [[providers]]
        name = "primary"
        kind = "openai"
        base_url = "{url}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#
    )
}

/// Primary with one chat fallback. `retry` attempts, threshold/cooldown for the
/// circuit tests.
fn fallback_config(
    primary: &str,
    fallback: &str,
    retry_max_attempts: u32,
    failure_threshold: u32,
    cooldown_ms: u64,
) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = {retry_max_attempts}
        retry_base_ms = 10
        retry_max_ms = 50
        circuit_failure_threshold = {failure_threshold}
        circuit_cooldown_ms = {cooldown_ms}

        [[providers]]
        name = "primary"
        kind = "openai"
        base_url = "{primary}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        fallbacks = ["claude-fb"]

        [[providers]]
        name = "fallback"
        kind = "openai"
        base_url = "{fallback}"
        [[providers.models]]
        id = "claude-fb"
        capabilities = ["chat"]
        "#
    )
}

// Criterion 1: 500, 500, 200 → success, exactly three upstream calls, and the
// two backoffs are actually waited.
#[tokio::test]
async fn retries_transient_5xx_then_succeeds() {
    let upstream = MockServer::start().await;
    // wiremock is first-mounted-wins: the 500 responder (capped at two hits)
    // takes the first two calls, then the 200 fall-through serves the third.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("gpt-4o")))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&single_provider_config(&upstream.uri()))).await;

    let started = Instant::now();
    let resp = post_chat(&base, "gpt").await;
    assert_eq!(resp.status(), 200);

    // Three upstream attempts (500, 500, 200).
    let hits = upstream.received_requests().await.unwrap().len();
    assert_eq!(hits, 3, "expected 3 upstream attempts, got {hits}");

    // Two equal-jitter backoffs (floors 10 ms + 20 ms) were actually waited.
    assert!(
        started.elapsed() >= Duration::from_millis(25),
        "backoff not respected: {:?}",
        started.elapsed()
    );
}

// Criterion 1b: a client 4xx is never retried.
#[tokio::test]
async fn client_4xx_is_not_retried() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&single_provider_config(&upstream.uri()))).await;
    let resp = post_chat(&base, "gpt").await;
    assert_eq!(resp.status(), 502); // upstream 4xx surfaces as LM-3003 (502)
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

// Criterion 2: primary exhausts retries → fallback serves, header names it.
#[tokio::test]
async fn falls_back_to_second_provider_and_advertises_it() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("claude")))
        .mount(&fallback)
        .await;

    let base = spawn(&config_from(&fallback_config(
        &primary.uri(),
        &fallback.uri(),
        2,
        5,
        30_000,
    )))
    .await;

    let resp = post_chat(&base, "gpt").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("claude-fb"),
        "response should name the fallback that served it"
    );
    // Primary tried twice (its retry budget), fallback once.
    assert_eq!(primary.received_requests().await.unwrap().len(), 2);
    assert_eq!(fallback.received_requests().await.unwrap().len(), 1);
}

// Criterion 3: 5 failures open the circuit; the next request skips the primary
// entirely; after the cooldown one probe is admitted.
#[tokio::test]
async fn circuit_opens_then_probes_after_cooldown() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("claude")))
        .mount(&fallback)
        .await;

    // One attempt per request (no retries), threshold 5, short cooldown.
    let base = spawn(&config_from(&fallback_config(
        &primary.uri(),
        &fallback.uri(),
        1,
        5,
        300,
    )))
    .await;

    // Five requests fail on the primary (each 1 attempt) → all served by the
    // fallback → breaker opens after the fifth.
    for _ in 0..5 {
        assert_eq!(post_chat(&base, "gpt").await.status(), 200);
    }
    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        5,
        "primary should have been hit exactly 5 times before opening"
    );

    // Sixth request: circuit open → primary skipped entirely, fallback serves.
    assert_eq!(post_chat(&base, "gpt").await.status(), 200);
    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        5,
        "open circuit must NOT touch the primary upstream"
    );

    // After the cooldown, exactly one probe reaches the primary again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(post_chat(&base, "gpt").await.status(), 200);
    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        6,
        "one half-open probe should reach the primary after cooldown"
    );
}

// Criterion 4: a streaming failure after chunks were emitted is NOT retried or
// failed over — the client gets a clean terminal SSE error frame.
#[tokio::test]
async fn streaming_failure_after_first_chunk_is_not_retried() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    // Two SSE data frames, then the body ends WITHOUT `[DONE]` (upstream died).
    let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(truncated),
        )
        .mount(&primary)
        .await;
    // The fallback must never be consulted once the stream has opened.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("claude")))
        .expect(0)
        .mount(&fallback)
        .await;

    let base = spawn(&config_from(&fallback_config(
        &primary.uri(),
        &fallback.uri(),
        3,
        5,
        30_000,
    )))
    .await;

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
    assert_eq!(resp.status(), 200); // headers already sent before the failure
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("gpt")
    );
    let body = resp.text().await.unwrap();
    // Both chunks forwarded, then a clean LM-3010 terminal error frame.
    assert!(body.contains("\"content\":\"a\""), "body: {body}");
    assert!(body.contains("\"content\":\"b\""), "body: {body}");
    assert!(
        body.contains("LM-3010"),
        "expected terminal error frame: {body}"
    );
    // Primary tried exactly once; the fallback's `.expect(0)` is verified on drop.
    assert_eq!(primary.received_requests().await.unwrap().len(), 1);
}

// Criterion 5: under a storm of upstream 429s, /health stays fast and every
// request completes (no unbounded queue, no hang).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_stays_fast_under_upstream_429_storm() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&upstream)
        .await;

    // One attempt each so the storm stays bounded; the point is gateway
    // stability, not upstream hammering.
    let cfg = format!(
        r#"
        [resilience]
        retry_max_attempts = 1

        [[providers]]
        name = "primary"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    // Fire 500 concurrent requests.
    let mut tasks = Vec::new();
    for _ in 0..500 {
        let base = base.clone();
        tasks.push(tokio::spawn(async move {
            post_chat(&base, "gpt").await.status()
        }));
    }

    // While the storm is in flight, /health must stay snappy.
    let client = reqwest::Client::new();
    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let t = Instant::now();
        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health.status(), 200);
        worst = worst.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        worst < Duration::from_millis(100),
        "/health degraded under load: worst {worst:?}"
    );

    // Every request completed with a fast failure, none hung. Under the storm
    // the breaker also trips (protecting the upstream), so a 503 (circuit open,
    // LM-3020) is as valid as the raw 429 — both prove the gateway shed load
    // without an unbounded queue.
    let mut completed = 0;
    for task in tasks {
        let status = task.await.unwrap();
        assert!(status == 429 || status == 503, "unexpected status {status}");
        completed += 1;
    }
    assert_eq!(completed, 500);
}

// Criterion 6: a 429 with `Retry-After: 1` forces at least a ~1 s wait before
// the retry (the header floors the backoff). Real time, kept to 1 s.
#[tokio::test]
async fn honours_retry_after_header_as_a_floor() {
    let upstream = MockServer::start().await;
    // First-mounted-wins: the 429 (capped at one hit) takes the first call,
    // then the 200 serves the retry.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("gpt-4o")))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&single_provider_config(&upstream.uri()))).await;

    let started = Instant::now();
    let resp = post_chat(&base, "gpt").await;
    assert_eq!(resp.status(), 200);
    // The Retry-After floor (1 s) dominates the ~10 ms base backoff.
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "Retry-After floor not honoured: {:?}",
        started.elapsed()
    );
}
