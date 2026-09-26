//! End-to-end HTTP tests for `POST /v1/systemone` (ADR 013): TypeSafe-wire
//! passthrough, alias resolution, edge validation (LM-2011 / LM-1001 before
//! any upstream call), routing misses, upstream error mapping, fallback on
//! `529 Overloaded`, ADR 003 usage (upstream vs flagged estimate), token
//! metrics, and client-disconnect handling. The upstream is a wiremock server
//! speaking TypeSafe's `/v1/systemone` schema.

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;
const KEY: &str = "ts-test-key-do-not-leak";

const ANSWERS: &str = r#"{"model":"jev-1.13.0","answers":{"is_urgent":{"type":"noul","noul":0.95},"department":{"type":"choice","choice":"billing","probabilities":{"technical":0.12,"billing":0.88},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;

/// A TypeSafe primary exposing `jev` (-> `jev-latest`) and a pinned
/// `jev-pinned` (-> `jev-1.13.0`) that falls back to a second TypeSafe
/// provider's `jev-fb`, plus a chat-only model for the capability miss.
fn config(primary: &str, fallback: &str) -> Config {
    let toml = format!(
        r#"
        [resilience]
        retry_max_attempts = 3
        retry_base_ms = 10
        retry_max_ms = 20

        [[providers]]
        name = "typesafe"
        kind = "typesafe"
        base_url = "{primary}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "jev"
        upstream_id = "jev-latest"
        capabilities = ["systemone"]
        cost_per_1m_input = 0.042
        [[providers.models]]
        id = "jev-pinned"
        upstream_id = "jev-1.13.0"
        capabilities = ["systemone"]
        fallbacks = ["jev-fb"]

        [[providers]]
        name = "typesafe-backup"
        kind = "typesafe"
        base_url = "{fallback}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "jev-fb"
        upstream_id = "jev-latest"
        capabilities = ["systemone"]

        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{primary}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#
    );
    Figment::new()
        .merge(Toml::string(&toml))
        .extract::<Config>()
        .expect("valid test config")
}

async fn spawn(primary: &str, fallback: &str) -> String {
    let config = config(primary, fallback);
    let mut specs = config.provider_specs();
    // Resolved keys are the caller's job; inject one directly (never via env,
    // so the tests stay parallel-safe).
    for spec in &mut specs {
        spec.api_key = Some(KEY.to_owned());
    }
    let registry = Arc::new(
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    );
    let state = common::base_state(registry)
        .with_pricing(CostTable::from_config(&config))
        .with_resilience(Arc::new(ResilienceRuntime::from_config(&config, None)));
    common::spawn_state(state, LIMIT).await
}

async fn mount_answers(upstream: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/json"))
        .mount(upstream)
        .await;
}

/// Unsorted `state` keys and choice options, so any key reordering shows.
const REQUEST: &str = r#"{"model":"jev","state":{"zeta":"Help! My payouts have been failing for 3 days.","alpha":1},"questions":{"is_urgent":{"type":"noul","instructions":"Does this convey urgency?"},"department":{"type":"choice","instructions":"Which team?","criteria":{"technical":"Bugs","billing":"Payments"}}}}"#;

async fn post(base: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/systemone"))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("send")
}

#[tokio::test]
async fn happy_path_passes_the_wire_through_and_resolves_the_alias() {
    let upstream = MockServer::start().await;
    mount_answers(&upstream, ANSWERS).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let resp = post(&base, REQUEST).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("jev")
    );
    let text = resp.text().await.expect("body");
    // Upstream-reported usage is unflagged, so the body is byte-identical.
    assert_eq!(text, ANSWERS);

    // The upstream got the client's bytes, with only `model` rewritten to the
    // upstream id: key order intact, nothing added.
    let received = upstream.received_requests().await.expect("recorded");
    let sent = std::str::from_utf8(&received[0].body).expect("utf8");
    assert_eq!(
        sent,
        REQUEST.replace(r#""model":"jev""#, r#""model":"jev-latest""#)
    );
    assert_eq!(
        received[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[tokio::test]
async fn missing_upstream_usage_is_estimated_and_flagged() {
    let upstream = MockServer::start().await;
    mount_answers(&upstream, r#"{"model":"jev-1.13.0","answers":{}}"#).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let resp = post(&base, REQUEST).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["usage"]["estimated"], true);
    assert!(body["usage"]["input_tokens"].as_u64().expect("count") > 0);
    assert_eq!(body["usage"]["output_tokens"], 0);

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape")
        .text()
        .await
        .expect("text");
    assert!(
        metrics.lines().any(|l| l.starts_with("lumen_tokens_total{")
            && l.contains(r#"capability="systemone""#)
            && l.contains(r#"direction="input""#)
            && l.contains(r#"estimated="true""#)),
        "{metrics}"
    );
}

#[tokio::test]
async fn upstream_usage_feeds_the_token_counters() {
    let upstream = MockServer::start().await;
    mount_answers(&upstream, ANSWERS).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;
    assert_eq!(post(&base, REQUEST).await.status(), 200);

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape")
        .text()
        .await
        .expect("text");
    let line = |direction: &str| {
        metrics
            .lines()
            .find(|l| {
                l.starts_with("lumen_tokens_total{")
                    && l.contains(r#"capability="systemone""#)
                    && l.contains(&format!(r#"direction="{direction}""#))
            })
            .map(str::to_owned)
    };
    let input = line("input").expect("input counter");
    assert!(input.ends_with(" 318"), "{input}");
    assert!(input.contains(r#"estimated="false""#), "{input}");
    let output = line("output").expect("output counter");
    assert!(output.ends_with(" 34"), "{output}");
}

#[tokio::test]
async fn contract_violations_are_rejected_before_any_upstream_call() {
    let upstream = MockServer::start().await;
    mount_answers(&upstream, ANSWERS).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let cases = [
        (
            r#"{"model":"jev","state":"s","questions":{}}"#,
            "LM-2011",
            "`questions`",
        ),
        (
            r#"{"model":"jev","state":"s","questions":{"q":{"type":"essay","instructions":"?"}}}"#,
            "LM-1001",
            "unknown type 'essay'",
        ),
        (
            r#"{"model":"jev","state":"s","questions":{"q":{"type":"choice","instructions":"?"}}}"#,
            "LM-1001",
            "requires `criteria`",
        ),
        (r#"{"model":"jev","questions":{}}"#, "LM-1001", "state"),
        (
            r#"{"model":"","state":"s","questions":{"q":{"type":"noul","instructions":"?"}}}"#,
            "LM-1001",
            "`model` must not be empty",
        ),
        (
            r#"{"model":"jev","state":"s","state":"t","questions":{}}"#,
            "LM-1001",
            "duplicate field `state`",
        ),
        ("not json", "LM-1001", ""),
    ];
    for (body, code, needle) in cases {
        let resp = post(&base, body).await;
        assert_eq!(resp.status(), 400, "{body}");
        let err: Value = resp.json().await.expect("json");
        assert_eq!(err["error"]["code"], code, "{body}: {err}");
        let message = err["error"]["message"].as_str().expect("message");
        assert!(message.contains(needle), "{body}: {message}");
    }
    assert!(upstream
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

#[tokio::test]
async fn routing_misses_use_the_standard_codes() {
    let upstream = MockServer::start().await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let resp = post(
        &base,
        &REQUEST.replace(r#""model":"jev""#, r#""model":"nope""#),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-2001");

    let resp = post(
        &base,
        &REQUEST.replace(r#""model":"jev""#, r#""model":"gpt""#),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-2002");
    assert!(err["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("systemone")));
}

#[tokio::test]
async fn upstream_422_is_a_502_not_retried_nor_failed_over() {
    // `jev-pinned` has 3 attempts and a fallback configured: a client-fault
    // 4xx must use neither (ADR 013 §4), and the key must not leak.
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"detail": KEY})))
        .mount(&primary)
        .await;
    let fallback = MockServer::start().await;
    mount_answers(&fallback, ANSWERS).await;
    let base = spawn(&primary.uri(), &fallback.uri()).await;

    let resp = post(
        &base,
        &REQUEST.replace(r#""model":"jev""#, r#""model":"jev-pinned""#),
    )
    .await;
    assert_eq!(resp.status(), 502);
    let text = resp.text().await.expect("body");
    assert!(!text.contains(KEY), "{text}");
    let err: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(err["error"]["code"], "LM-3003");
    assert!(err["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("'typesafe'")));
    assert_eq!(
        primary.received_requests().await.expect("recorded").len(),
        1
    );
    assert!(fallback
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

#[tokio::test]
async fn overloaded_529_falls_back_and_the_header_names_the_fallback() {
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(529))
        .mount(&primary)
        .await;
    let fallback = MockServer::start().await;
    mount_answers(&fallback, ANSWERS).await;
    let base = spawn(&primary.uri(), &fallback.uri()).await;

    let resp = post(
        &base,
        &REQUEST.replace(r#""model":"jev""#, r#""model":"jev-pinned""#),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("jev-fb")
    );

    let sent = primary.received_requests().await.expect("recorded");
    let primary_body: Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(primary_body["model"], "jev-1.13.0");
    let sent = fallback.received_requests().await.expect("recorded");
    let fallback_body: Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(fallback_body["model"], "jev-latest");
}

#[tokio::test]
async fn models_list_advertises_the_systemone_capability() {
    let upstream = MockServer::start().await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;
    let body: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let jev = body["data"]
        .as_array()
        .expect("list")
        .iter()
        .find(|m| m["id"] == "jev")
        .expect("jev listed");
    assert_eq!(jev["capabilities"], json!(["systemone"]));
    assert_eq!(jev["owned_by"], "typesafe");
}

#[tokio::test]
async fn client_disconnect_during_slow_upstream_does_not_hang_server() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(ANSWERS, "application/json")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&upstream)
        .await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    // The client gives up well before the upstream answers; dropping the
    // connection drops the handler future and cancels the upstream call.
    let result = reqwest::Client::new()
        .post(format!("{base}/v1/systemone"))
        .timeout(Duration::from_millis(200))
        .header("content-type", "application/json")
        .body(REQUEST)
        .send()
        .await;
    assert!(result.is_err(), "client should have timed out");
    assert_eq!(
        upstream.received_requests().await.expect("recorded").len(),
        1
    );

    let health = reqwest::get(format!("{base}/health"))
        .await
        .expect("health");
    assert_eq!(health.status(), 200);
}
