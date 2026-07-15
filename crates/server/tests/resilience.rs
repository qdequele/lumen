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

use figment::{
    providers::{Format, Toml},
    Figment,
};
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
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

/// Primary (OpenAI, accepts remote image URLs) with a Gemini fallback (which
/// only takes inline base64 image data). The primary model declares the
/// `image` modality so a remote image URL clears the LM-2003 vision-capable
/// gate; only the fallback link cannot serve it.
fn image_capable_primary_with_gemini_fallback_config(primary: &str, fallback: &str) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = 1
        retry_base_ms = 5
        retry_max_ms = 20

        [[providers]]
        name = "primary"
        kind = "openai"
        base_url = "{primary}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        modalities = ["text", "image"]
        fallbacks = ["gemini-fb"]

        [[providers]]
        name = "fallback"
        kind = "google"
        base_url = "{fallback}"
        [[providers.models]]
        id = "gemini-fb"
        capabilities = ["chat"]
        modalities = ["text", "image"]
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

// GH #13 / LM-2004: the primary (OpenAI) accepts a remote image URL, so the
// LM-2004 pre-flight (which only inspects `chain[0]`) lets the request
// through. The primary then fails and the chain falls back to Gemini, which
// cannot take a remote URL. That must surface as the honest LM-2004 client
// error (400), not an upstream LM-3002 (502) - a fail-over is not the
// client's fault when it happens, but *this specific* failure (an
// image-incapable fallback) is a client-input problem, not the fallback
// provider's fault.
#[tokio::test]
async fn fallback_incapable_of_remote_image_url_is_lm_2004_not_502() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    // Primary always fails, forcing a fail-over to the Gemini fallback. The
    // fallback mock is never given a mapping - it must never be hit.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&primary)
        .await;

    let base = spawn(&config_from(
        &image_capable_primary_with_gemini_fallback_config(&primary.uri(), &fallback.uri()),
    ))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"https://example.com/x.png"}}
            ]}]
        }))
        .send()
        .await
        .expect("request sent");

    assert_eq!(
        resp.status(),
        400,
        "an image-incapable fallback must surface as a client error, not a 502"
    );
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["error"]["code"], "LM-2004");

    // The primary was tried (and exhausted its retry budget); the fallback
    // was never actually contacted over HTTP - the remote URL is rejected in
    // translation, before any request leaves the gateway.
    assert!(!primary.received_requests().await.unwrap().is_empty());
    assert!(fallback.received_requests().await.unwrap().is_empty());
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
// failed over - the client gets a clean terminal SSE error frame.
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

/// Raise this process's soft fd limit towards its hard limit.
///
/// The 429 storm below holds on the order of 2000 sockets at peak, all in this
/// one test process (storm client + gateway inbound + gateway outbound +
/// wiremock inbound). macOS's launchd default soft limit is 256, which turns
/// the storm into EMFILE noise unrelated to what the test asserts.
#[cfg(unix)]
fn raise_fd_limit() {
    // SAFETY: getrlimit/setrlimit only read/write the plain rlimit struct
    // passed by pointer; no ownership or aliasing concerns.
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) == 0 {
            let want = lim.rlim_max.min(8192);
            if lim.rlim_cur < want {
                lim.rlim_cur = want;
                // Best effort: if the kernel refuses, the connect retry in the
                // storm still absorbs transient socket-exhaustion failures.
                let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &raw const lim);
            }
        }
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

/// True when a request error is the kernel refusing the connection itself:
/// a connect-phase failure, or a reset/broken-pipe surfaced right after the
/// handshake. Under listen-queue overflow macOS completes the handshake and
/// THEN sends RST, so the failure lands either at connect (`ECONNRESET`) or
/// on the first write (`EPIPE`), depending on timing.
fn kernel_conn_failure(e: &reqwest::Error) -> bool {
    if e.is_connect() {
        return true;
    }
    let mut source = std::error::Error::source(e);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            );
        }
        source = inner.source();
    }
    false
}

/// POST one storm request, retrying kernel-level connection failures.
///
/// macOS clamps every listen backlog to `kern.ipc.somaxconn` (128 by default)
/// and answers accept-queue overflow with RST, so a 500-way simultaneous
/// connect burst on loopback gets some handshakes reset before the gateway
/// ever sees them (Linux silently drops the SYN and the client's TCP stack
/// retransmits; this bounded retry gives macOS the same semantics). Only
/// kernel-level connection failures are retried: an HTTP-level error or a
/// hang still fails the test, so this cannot mask a wedged gateway - and a
/// gateway that resets every connection exhausts the bounded budget anyway.
async fn storm_chat(client: &reqwest::Client, base: &str) -> reqwest::StatusCode {
    let mut attempts: u32 = 0;
    loop {
        let sent = client
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
            .send()
            .await;
        match sent {
            Ok(resp) => return resp.status(),
            Err(e) if kernel_conn_failure(&e) && attempts < 100 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("storm request failed after {attempts} connect retries: {e:?}"),
        }
    }
}

// Criterion 5: under a storm of upstream 429s, /health stays fast and every
// request completes (no unbounded queue, no hang).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_stays_fast_under_upstream_429_storm() {
    raise_fd_limit();
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

    // Establish the health probe's keep-alive connection BEFORE the storm, as
    // a monitoring agent would. The latency loop below then measures the
    // gateway's handler path, not the kernel's (somaxconn-clamped) accept
    // queue that the storm is about to slam.
    let client = reqwest::Client::new();
    let warmup = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health warm-up");
    assert_eq!(warmup.status(), 200);

    // Fire 500 concurrent requests through one pooled client. A single client
    // (rather than one per task) lets finished connections be reused by
    // still-queued requests instead of piling more handshakes onto the
    // backlog, and the timeout turns a wedged gateway into a loud failure
    // instead of a hung test.
    let storm_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("storm client");
    let mut tasks = Vec::new();
    for _ in 0..500 {
        let base = base.clone();
        let storm_client = storm_client.clone();
        tasks.push(tokio::spawn(async move {
            storm_chat(&storm_client, &base).await
        }));
    }

    // While the storm is in flight, /health must stay snappy.
    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let t = Instant::now();
        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health.status(), 200);
        worst = worst.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // A genuinely regressed /health - one that touched the DB, a provider, or
    // shared the storm's bounded queue - would serialize behind the 500 in-flight
    // requests and take seconds. This bound proves it stayed off that path while
    // tolerating the jitter of a shared CI runner (locally it's well under 10 ms).
    assert!(
        worst < Duration::from_millis(750),
        "/health degraded under load: worst {worst:?}"
    );

    // Every request completed with a fast failure, none hung. Under the storm
    // the breaker also trips (protecting the upstream), so a 503 (circuit open,
    // LM-3020) is as valid as the raw 429 - both prove the gateway shed load
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
