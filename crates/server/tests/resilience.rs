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
        Registry::build(
            config.provider_specs(),
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
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

/// POST a chat request, retrying a handful of times on a client-side
/// transport-level failure (never on a received HTTP response) before
/// giving up.
///
/// Under `health_stays_fast_under_upstream_429_storm`'s hundreds of
/// concurrent connects, a saturated OS accept backlog can reset a fraction of
/// the TCP handshakes outright (`is_connect()`) or drop the connection while
/// the request body is still being written, surfacing as a broken-pipe
/// `SendRequest` error (`is_request()`) - both are artifacts of the test
/// host's networking stack under a burst this size, not gateway behaviour,
/// and neither is what any test in this file asserts on. Retrying only these
/// pre-response transport failures keeps every status-code assertion exactly
/// as strict as before; it just stops a transient reset from panicking the
/// whole task.
async fn post_chat(base: &str, model: &str) -> reqwest::Response {
    const MAX_TRANSPORT_RETRIES: u32 = 8;
    let body = json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] });
    let mut attempt = 0;
    loop {
        match reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => return resp,
            Err(err)
                if (err.is_connect() || err.is_request()) && attempt < MAX_TRANSPORT_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(20 * u64::from(attempt))).await;
            }
            Err(err) => panic!("request send failed after {attempt} transport retries: {err:?}"),
        }
    }
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
        id = "primary/gpt"
        capabilities = ["chat"]
        "#
    )
}

/// Primary with one chat fallback, as the virtual model `gpt` over the
/// foundation models `primary-gpt` and `claude-fb`. `retry` attempts, threshold/cooldown for the
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
        id = "primary/primary-gpt"
        upstream_id = "gpt"
        capabilities = ["chat"]

        [[providers]]
        name = "fallback"
        kind = "openai"
        base_url = "{fallback}"
        [[providers.models]]
        id = "fallback/claude-fb"
        capabilities = ["chat"]

        [[virtual_models]]
        id = "lumen/gpt"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "primary/primary-gpt" }}, {{ model = "fallback/claude-fb" }}]
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
        id = "primary/primary-gpt"
        upstream_id = "gpt"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[providers]]
        name = "fallback"
        kind = "google"
        base_url = "{fallback}"
        [[providers.models]]
        id = "fallback/gemini-fb"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[virtual_models]]
        id = "lumen/gpt"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "primary/primary-gpt" }}, {{ model = "fallback/gemini-fb" }}]
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
        Some("fallback/claude-fb"),
        "response should name the fallback that served it"
    );
    // Primary tried twice (its retry budget), fallback once.
    assert_eq!(primary.received_requests().await.unwrap().len(), 2);
    assert_eq!(fallback.received_requests().await.unwrap().len(), 1);
}

/// POST a one-image chat request for `model` whose image is `url`.
async fn post_image_chat(base: &str, model: &str, url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": model,
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url": url}}
            ]}]
        }))
        .send()
        .await
        .expect("request sent")
}

/// A server that answers every POST with `status`.
async fn always(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

// GH #13 / LM-2004: the primary (OpenAI) accepts a remote image URL, so the
// request clears the LM-2004 pre-flight, which judges the primary. A Gemini
// fallback cannot fetch a remote URL: it is not an attempt for this request,
// exactly as a text-only fallback is not one for an image. The fail-over goes
// on to the next target that can take the URL.
#[tokio::test]
async fn fallback_incapable_of_remote_image_url_is_skipped() {
    let primary = always(503).await;
    let gemini = always(503).await;
    let third = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("gpt")))
        .mount(&third)
        .await;
    let config = format!(
        r#"{}
        [[providers]]
        name = "third"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "third/third-gpt"
        upstream_id = "gpt"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[virtual_models]]
        id = "lumen/three"
        capability = "chat"
        strategy = "fallback"
        targets = [
            {{ model = "primary/primary-gpt" }},
            {{ model = "fallback/gemini-fb" }},
            {{ model = "third/third-gpt" }},
        ]
        "#,
        image_capable_primary_with_gemini_fallback_config(&primary.uri(), &gemini.uri()),
        third.uri()
    );
    let base = spawn(&config_from(&config)).await;

    let resp = post_image_chat(&base, "lumen/three", "https://example.com/x.png").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("third/third-gpt")
    );
    assert!(!primary.received_requests().await.unwrap().is_empty());
    assert!(
        gemini.received_requests().await.unwrap().is_empty(),
        "a target that cannot fetch the URL must never be sent it"
    );

    // An inline `data:` image is something Gemini can take: the same chain
    // does fail over to it.
    let _ = post_image_chat(&base, "lumen/three", "data:image/png;base64,AAAA").await;
    assert!(
        !gemini.received_requests().await.unwrap().is_empty(),
        "an inline image still fails over to the Gemini target"
    );
}

// When the only fallback cannot take the remote URL, nothing is left after
// the primary: its own upstream failure is what the client sees (not an
// `LM-2004` blaming the client for a fail-over it did not choose).
#[tokio::test]
async fn primary_failure_surfaces_when_no_fallback_can_take_the_remote_image_url() {
    let primary = always(503).await;
    let gemini = always(200).await;
    let base = spawn(&config_from(
        &image_capable_primary_with_gemini_fallback_config(&primary.uri(), &gemini.uri()),
    ))
    .await;

    let resp = post_image_chat(&base, "gpt", "https://example.com/x.png").await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["error"]["code"], "LM-3003");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("'primary'")),
        "{body}"
    );
    assert!(!primary.received_requests().await.unwrap().is_empty());
    assert!(gemini.received_requests().await.unwrap().is_empty());
}

// LM-2008 follows the same rule: a provider-native image source is judged on
// the primary, and a fallback that cannot resolve it is skipped. An OpenAI
// fallback would otherwise be sent `anthropic-file:...` or a Gemini file URI
// verbatim as an image URL.
#[tokio::test]
async fn fallback_incapable_of_a_provider_native_image_source_is_skipped() {
    for (kind, url) in [
        ("anthropic", "anthropic-file:file_abc123"),
        (
            "google",
            "https://generativelanguage.googleapis.com/v1beta/files/abc123",
        ),
        ("google", "gs://bucket/cat.png"),
    ] {
        let primary = always(503).await;
        let fallback = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("gpt")))
            .mount(&fallback)
            .await;
        let config = format!(
            r#"
            [resilience]
            retry_max_attempts = 1
            retry_base_ms = 5
            retry_max_ms = 20

            [[providers]]
            name = "primary"
            kind = "{kind}"
            base_url = "{}"
            [[providers.models]]
            id = "primary/native"
            capabilities = ["chat"]
            modalities = ["text", "image"]

            [[providers]]
            name = "fallback"
            kind = "openai"
            base_url = "{}"
            [[providers.models]]
            id = "fallback/gpt-fb"
            upstream_id = "gpt"
            capabilities = ["chat"]
            modalities = ["text", "image"]

            [[virtual_models]]
            id = "lumen/native"
            capability = "chat"
            strategy = "fallback"
            targets = [{{ model = "primary/native" }}, {{ model = "fallback/gpt-fb" }}]
            "#,
            primary.uri(),
            fallback.uri()
        );
        let base = spawn(&config_from(&config)).await;

        let resp = post_image_chat(&base, "lumen/native", url).await;
        assert_ne!(resp.status(), 200, "{kind} {url}");
        let body: Value = resp.json().await.expect("json body");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("'primary'")),
            "{kind} {url}: {body}"
        );
        assert!(
            fallback.received_requests().await.unwrap().is_empty(),
            "{kind} {url}: the fallback cannot resolve this source"
        );

        // A plain inline image still fails over to the OpenAI target.
        let resp = post_image_chat(&base, "lumen/native", "data:image/png;base64,AAAA").await;
        assert_eq!(resp.status(), 200, "{kind}");
    }
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
        Some("primary/primary-gpt")
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

// First-frame-peek (issue #7, ADR 005 2026-07-15): a stream that opens 200 then
// closes before any content frame is a PRE-COMMIT failure. The peek fails it
// over to the fallback (client sees a clean successful stream) AND penalises the
// primary's breaker - proven here by a second request that skips the primary
// entirely (threshold 1). Contrast with `streaming_failure_after_first_chunk_is_
// not_retried` above, where a content frame already committed the response.
#[tokio::test]
async fn streaming_empty_first_frame_falls_over_and_penalises_the_breaker() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    // 200 + event-stream headers, but an EMPTY body: the stream opens then ends
    // before delivering a single content frame.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream"))
        .mount(&primary)
        .await;
    // A healthy SSE stream: one content frame then the terminator.
    let good = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(good),
        )
        .mount(&fallback)
        .await;

    // One attempt per link (no retries), breaker opens after a single failure.
    let base = spawn(&config_from(&fallback_config(
        &primary.uri(),
        &fallback.uri(),
        1,
        1,
        30_000,
    )))
    .await;

    let stream_once = || async {
        reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({
                "model": "gpt",
                "messages": [{ "role": "user", "content": "hi" }],
                "stream": true
            }))
            .send()
            .await
            .unwrap()
    };

    // Request 1: primary empty-streams → peek fails over to the fallback, and the
    // client gets a clean, successful stream with no error frame.
    let resp = stream_once().await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("fallback/claude-fb"),
        "the peek must fall over to the fallback"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"content\":\"hello\""), "body: {body}");
    assert!(body.contains("[DONE]"), "body: {body}");
    assert!(!body.contains("LM-3"), "no error frame expected: {body}");

    // Request 2: the primary's breaker was penalised by the failed peek and is
    // now open, so the primary is skipped entirely (its hit count stays at 1).
    let resp = stream_once().await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("fallback/claude-fb")
    );

    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        1,
        "the failed peek must open the breaker so the primary is skipped next time"
    );
    assert_eq!(fallback.received_requests().await.unwrap().len(), 2);
}

// First-frame-peek (issue #7): a silent upstream that opens but sends no bytes
// must fail over on the first_token timeout, not hang the peek. (wiremock cannot
// send headers then stall the body, so the whole response is delayed past the
// first_token window; the peek-window bound itself is unit-tested in
// `router::peek`.)
#[tokio::test]
async fn streaming_silent_upstream_first_token_times_out_then_falls_over() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    // Primary "opens" but only after a delay far beyond the first_token window.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&primary)
        .await;
    let good = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(good),
        )
        .mount(&fallback)
        .await;

    // A short first_token so the silent primary trips it quickly; one attempt.
    let cfg = format!(
        r#"
        [server]
        first_token_timeout_ms = 300

        [resilience]
        retry_max_attempts = 1
        retry_base_ms = 10
        retry_max_ms = 50

        [[providers]]
        name = "primary"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "primary/primary-gpt"
        upstream_id = "gpt"
        capabilities = ["chat"]

        [[providers]]
        name = "fallback"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "fallback/claude-fb"
        capabilities = ["chat"]

        [[virtual_models]]
        id = "lumen/gpt"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "primary/primary-gpt" }}, {{ model = "fallback/claude-fb" }}]
        "#,
        primary.uri(),
        fallback.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    let started = Instant::now();
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
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("fallback/claude-fb"),
        "the silent primary must fail over on the first_token timeout"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"content\":\"hi\""), "body: {body}");
    // It failed over promptly (well under the primary's 5 s delay).
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "did not fail over on the first_token timeout: {:?}",
        started.elapsed()
    );
}

/// Concurrent-request count for the storm test. Defaults to the CI-scale
/// 500 (unchanged); override with `LUMEN_RESILIENCE_STORM_SIZE` on a host
/// whose OS accept backlog can't tolerate that many near-simultaneous
/// connects (see `storm_chat`'s kernel-level connect-retry for the primary
/// fix - this is an escape hatch for hosts where even that isn't enough
/// headroom).
fn storm_size() -> usize {
    std::env::var("LUMEN_RESILIENCE_STORM_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
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
        id = "primary/gpt"
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

    // Fire the storm (CI-scale default 500; see `storm_size`) through one
    // pooled client. A single client (rather than one per task) lets finished
    // connections be reused by still-queued requests instead of piling more
    // handshakes onto the backlog, and the timeout turns a wedged gateway
    // into a loud failure instead of a hung test.
    let storm_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("storm client");
    let size = storm_size();
    let mut tasks = Vec::with_capacity(size);
    for _ in 0..size {
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
    assert_eq!(completed, size);
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

// ---------------------------------------------------------------------------
// Cold start (ADR 021): a scale-to-zero upstream answers 503 until it wakes up.
// ---------------------------------------------------------------------------

/// One OpenAI-kind provider serving chat + embed and one Cohere-kind provider
/// serving rerank, both on `url`, small backoffs. `extra` lands in
/// `[resilience]`, `provider_extra` on the OpenAI-kind provider.
fn cold_start_config(url: &str, extra: &str, provider_extra: &str) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = 3
        retry_base_ms = 10
        retry_max_ms = 50
        {extra}

        [[providers]]
        name = "hf"
        kind = "openai"
        base_url = "{url}"
        {provider_extra}
        [[providers.models]]
        id = "hf/bge-m3"
        capabilities = ["chat", "embed"]

        [[providers]]
        name = "rr"
        kind = "cohere"
        base_url = "{url}"
        [[providers.models]]
        id = "rr/rerank"
        upstream_id = "rerank-v3.5"
        capabilities = ["rerank"]
        "#
    )
}

fn embed_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "object": "list",
        "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
        "model": "bge-m3",
        "usage": { "prompt_tokens": 4, "total_tokens": 4 }
    }))
}

/// Hugging Face's loading response: 503 with `estimated_time` in seconds.
fn hf_loading(estimated_time: f64) -> ResponseTemplate {
    ResponseTemplate::new(503).set_body_json(json!({
        "error": "Model BAAI/bge-m3 is currently loading",
        "estimated_time": estimated_time
    }))
}

async fn post_embed(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "hf/bge-m3", "input": "abcd" }))
        .send()
        .await
        .expect("send embeddings")
}

/// The audit case: the first embeddings call after idle gets HF's 503
/// loading response. The gateway waits the `estimated_time` (1 s, far past
/// the 10 ms base backoff) and the retry succeeds: the client sees one 200.
#[tokio::test]
async fn embeddings_cold_start_waits_for_hf_estimated_time() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(hf_loading(1.0))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(embed_ok())
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(&upstream.uri(), "", ""))).await;
    let started = Instant::now();
    let resp = post_embed(&base).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["usage"]["prompt_tokens"], 4,
        "usage of the one success"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "estimated_time not honoured: {:?}",
        started.elapsed()
    );
}

/// A 503 `Retry-After` header is a backoff floor on rerank too.
#[tokio::test]
async fn rerank_503_retry_after_header_is_honoured() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 0, "relevance_score": 0.9 }],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(&upstream.uri(), "", ""))).await;
    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rr/rerank", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "Retry-After floor not honoured on 503: {:?}",
        started.elapsed()
    );
}

/// A streaming chat whose open is answered 503 is retried (nothing reached
/// the client yet) after the hinted wait, and the stream then flows.
#[tokio::test]
async fn streaming_chat_503_at_open_is_retried_before_any_frame() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(&upstream.uri(), "", ""))).await;
    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "hf/bge-m3",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"content\":\"hi\""), "body: {body}");
    assert!(body.contains("[DONE]"), "body: {body}");
    assert!(!body.contains("LM-"), "no error frame: {body}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert!(started.elapsed() >= Duration::from_millis(950));
}

/// A wait hint longer than `retry_after_max_ms` is not waited for: the 503 is
/// returned at once (LM-3003 naming the provider), after one upstream call.
#[tokio::test]
async fn wait_hint_above_the_cap_is_not_waited_for() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "120"))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(
        &upstream.uri(),
        "retry_after_max_ms = 30000",
        "",
    )))
    .await;
    let started = Instant::now();
    let resp = post_embed(&base).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3003", "body: {body}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait for a hint above the cap: {:?}",
        started.elapsed()
    );
}

/// When the hinted wait would end past the total deadline, the gateway does
/// not sleep into a certain LM-3013: it returns the upstream 503 (LM-3003).
#[tokio::test]
async fn backoff_past_the_total_deadline_returns_the_upstream_error() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(hf_loading(5.0))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(
        &upstream.uri(),
        "",
        "total_timeout_ms = 2000",
    )))
    .await;
    let started = Instant::now();
    let resp = post_embed(&base).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3003", "body: {body}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_millis(1500));
}

/// A provider without wait hints gets its own, longer retry policy; the
/// global one (a single attempt here) is untouched for other providers.
#[tokio::test]
async fn per_provider_retry_policy_outlasts_a_hintless_cold_start() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(3)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(embed_ok())
        .mount(&upstream)
        .await;
    // Rerank on the other provider: a single attempt (global policy).
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;

    let config = format!(
        "{}\n",
        cold_start_config(
            &upstream.uri(),
            "",
            "retry_max_attempts = 5\nretry_base_ms = 20\nretry_max_ms = 40",
        )
    )
    .replace("retry_max_attempts = 3\n", "retry_max_attempts = 1\n");
    let base = spawn(&config_from(&config)).await;

    let resp = post_embed(&base).await;
    assert_eq!(resp.status(), 200);
    let embed_hits = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/embeddings")
        .count();
    assert_eq!(embed_hits, 4, "503 x3 then 200 on the provider's policy");

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rr/rerank", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let rerank_hits = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v2/rerank")
        .count();
    assert_eq!(rerank_hits, 1, "the global single attempt still applies");
}

/// The cap applies to a 429 too (behaviour change, ADR 021): a
/// `Retry-After: 120` above a 30 s cap is returned at once as LM-3001.
#[tokio::test]
async fn rate_limit_hint_above_the_cap_is_not_waited_for() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&upstream)
        .await;

    let base = spawn(&config_from(&cold_start_config(
        &upstream.uri(),
        "retry_after_max_ms = 30000",
        "",
    )))
    .await;
    let started = Instant::now();
    let resp = post_embed(&base).await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3001", "body: {body}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}
