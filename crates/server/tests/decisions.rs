//! End-to-end HTTP tests for `POST /v1/decisions` (ADR 016) and its
//! deprecated alias `/v1/systemone`: both edge formats (OpenAI and TypeSafe),
//! TypeSafe-wire passthrough, alias resolution, edge validation (LM-2011 /
//! LM-1001 before any upstream call), routing misses, upstream error mapping,
//! fallback on `529 Overloaded` (also across vendors in both directions),
//! target compatibility skips (images, a choice of one option), ADR 003 usage
//! (upstream vs flagged estimate), token and decisions metrics, deprecation
//! headers, and client-disconnect handling. The upstreams are wiremock
//! servers: TypeSafe's `/v1/systemone`, OpenAI's and Perplexity's
//! `/v1/decisions`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use lumen_telemetry::ResilienceMetrics;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;
const KEY: &str = "ts-test-key-do-not-leak";

const ANSWERS: &str = r#"{"model":"jev-1.13.0","answers":{"is_urgent":{"type":"noul","noul":0.95},"department":{"type":"choice","choice":"billing","probabilities":{"technical":0.12,"billing":0.88},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;

/// The upstream mock servers of one test.
struct Upstreams<'a> {
    /// TypeSafe primary (`jev`, `jev-pinned-leaf`).
    primary: &'a str,
    /// TypeSafe backup (`jev-fb`).
    fallback: &'a str,
    /// OpenAI (`luna`), at `{luna}/v1/decisions`.
    luna: &'a str,
    /// Perplexity (`decider`), at `{decider}/v1/decisions`.
    decider: &'a str,
    /// Consecutive failures that open a circuit breaker.
    circuit_failure_threshold: u32,
}

impl<'a> Upstreams<'a> {
    /// Every vendor on one mock (the paths keep TypeSafe apart; OpenAI and
    /// Perplexity both answer `/v1/decisions`).
    fn one(uri: &'a str) -> Self {
        Self::two(uri, uri)
    }

    fn two(primary: &'a str, fallback: &'a str) -> Self {
        Self {
            primary,
            fallback,
            luna: primary,
            decider: primary,
            circuit_failure_threshold: 5,
        }
    }
}

/// A TypeSafe primary exposing `jev` (-> `jev-latest`) and a pinned
/// `jev-pinned-leaf` (-> `jev-1.13.0`); the virtual model `jev-pinned` falls
/// back from it to a second TypeSafe provider's `jev-fb`, and the virtual
/// model `jev-rerank` reranks through `jev` with a remap (ADR 014). An OpenAI
/// provider serves `luna` (text and image) next to a chat-only model (for the
/// capability miss), a Perplexity provider serves `decider` (text only, the
/// config default), and the virtual models `cross` (`decider`, `jev`,
/// `luna`) and `cross-luna-first` (`luna`, `decider`, `jev`) fall back across
/// vendors.
fn config(up: &Upstreams<'_>) -> Config {
    let Upstreams {
        primary,
        fallback,
        luna,
        decider,
        circuit_failure_threshold,
    } = up;
    let toml = format!(
        r#"
        [resilience]
        retry_max_attempts = 3
        retry_base_ms = 10
        retry_max_ms = 20
        circuit_failure_threshold = {circuit_failure_threshold}

        [[providers]]
        name = "typesafe"
        kind = "typesafe"
        base_url = "{primary}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "jev"
        upstream_id = "jev-latest"
        capabilities = ["decisions"]
        cost_per_1m_input = 0.042
        [[providers.models]]
        id = "jev-pinned-leaf"
        upstream_id = "jev-1.13.0"
        capabilities = ["decisions"]

        [[providers]]
        name = "typesafe-backup"
        kind = "typesafe"
        base_url = "{fallback}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "jev-fb"
        upstream_id = "jev-latest"
        capabilities = ["decisions"]

        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{luna}/v1"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        [[providers.models]]
        id = "luna"
        upstream_id = "gpt-6-luna"
        capabilities = ["decisions"]
        modalities = ["text", "image"]
        cost_per_1m_input = 0.10

        [[providers]]
        name = "perplexity"
        kind = "perplexity"
        base_url = "{decider}"
        api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
        [[providers.models]]
        id = "decider"
        upstream_id = "decider-1"
        capabilities = ["decisions"]
        cost_per_1m_input = 0.02

        [[virtual_models]]
        id = "jev-pinned"
        capability = "decisions"
        strategy = "fallback"
        targets = [{{ model = "jev-pinned-leaf" }}, {{ model = "jev-fb" }}]

        [[virtual_models]]
        id = "cross"
        capability = "decisions"
        strategy = "fallback"
        targets = [{{ model = "decider" }}, {{ model = "jev" }}, {{ model = "luna" }}]

        [[virtual_models]]
        id = "cross-luna-first"
        capability = "decisions"
        strategy = "fallback"
        targets = [{{ model = "luna" }}, {{ model = "decider" }}, {{ model = "jev" }}]

        [[virtual_models]]
        id = "jev-rerank"
        capability = "rerank"
        strategy = "single"
        targets = [{{ model = "jev", remap = {{ instructions = "Could `document` be the cited precedent?", criteria.true = "States the cited rule." }} }}]
        "#
    );
    Figment::new()
        .merge(Toml::string(&toml))
        .extract::<Config>()
        .expect("valid test config")
}

async fn spawn(primary: &str, fallback: &str) -> String {
    spawn_upstreams(&Upstreams::two(primary, fallback)).await
}

async fn spawn_upstreams(up: &Upstreams<'_>) -> String {
    spawn_with_resilience(up).await.0
}

/// Spawn and keep a handle on the resilience runtime (circuit breakers),
/// whose state gauge is exported on `/metrics`.
async fn spawn_with_resilience(up: &Upstreams<'_>) -> (String, Arc<ResilienceRuntime>) {
    let config = config(up);
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
    let state = common::base_state(registry);
    let resilience_metrics =
        ResilienceMetrics::register(&state.metrics).expect("register resilience metrics");
    let resilience = Arc::new(ResilienceRuntime::from_config(
        &config,
        Some(resilience_metrics),
    ));
    let state = state
        .with_pricing(CostTable::from_config(&config))
        .with_resilience(resilience.clone());
    (common::spawn_state(state, LIMIT).await, resilience)
}

async fn mount_answers(upstream: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/json"))
        .mount(upstream)
        .await;
}

/// [`ANSWERS`] without `usage`.
const ANSWERS_NO_USAGE: &str = r#"{"model":"jev-1.13.0","answers":{"is_urgent":{"type":"noul","noul":0.95},"department":{"type":"choice","choice":"billing","probabilities":{"technical":0.12,"billing":0.88},"confidence":0.81}}}"#;

/// Unsorted `state` keys and choice options, so any key reordering shows.
const REQUEST: &str = r#"{"model":"jev","state":{"zeta":"Help! My payouts have been failing for 3 days.","alpha":1},"questions":{"is_urgent":{"type":"noul","instructions":"Does this convey urgency?"},"department":{"type":"choice","instructions":"Which team?","criteria":{"technical":"Bugs","billing":"Payments"}}}}"#;

async fn post(base: &str, body: &str) -> reqwest::Response {
    post_to(base, "/v1/decisions", body).await
}

async fn post_to(base: &str, route: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{route}"))
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
    mount_answers(&upstream, ANSWERS_NO_USAGE).await;
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
            && l.contains(r#"capability="decisions""#)
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
                    && l.contains(r#"capability="decisions""#)
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
        (
            r#"{"model":"jev","questions":{}}"#,
            "LM-1001",
            "send either OpenAI format",
        ),
        (
            r#"{"model":"jev","input":"x","state":"s","questions":{}}"#,
            "LM-1001",
            "send either OpenAI format",
        ),
        (
            r#"{"model":"jev","input":"x","questions":{"q":{"type":"predicate"}}}"#,
            "LM-1001",
            "send either OpenAI format",
        ),
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
        .is_some_and(|m| m.contains("decisions")));
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
async fn models_list_advertises_the_decisions_capability() {
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
    assert_eq!(jev["capabilities"], json!(["decisions"]));
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
        .post(format!("{base}/v1/decisions"))
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

// ---- Jev as a reranker (ADR 013 amendment) ----------------------------------

/// Answers each noul question with the `#score` suffix of its document.
struct EchoScores;

impl Respond for EchoScores {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("json body");
        let questions = body["questions"].as_object().expect("questions");
        let answers: serde_json::Map<String, Value> = questions
            .iter()
            .map(|(id, q)| {
                let doc = q["instructions"]["document"].as_str().unwrap_or_default();
                let score: f64 = doc
                    .rsplit('#')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                (id.clone(), json!({"type": "noul", "noul": score}))
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 100, "output_tokens": 3}
        }))
    }
}

#[tokio::test]
async fn jev_serves_v1_rerank_through_a_virtual_model_remap() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(EchoScores)
        .mount(&upstream)
        .await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "jev-rerank",
            "query": "cited precedent",
            "documents": ["low #0.1", "high #0.9", {"text": "mid #0.5"}],
            "top_n": 2,
            "return_documents": true
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lumen-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("jev"),
        "the leaf that served the virtual reranker"
    );
    let body: Value = resp.json().await.expect("json");
    // Sorted by Jev's noul, top_n applied, documents echoed by original index.
    assert_eq!(body["results"].as_array().expect("results").len(), 2);
    assert_eq!(body["results"][0]["index"], 1);
    assert_eq!(body["results"][0]["document"]["text"], "high #0.9");
    assert_eq!(body["results"][1]["index"], 2);
    // Jev's upstream token count, unflagged; search units derived.
    assert_eq!(body["usage"]["total_tokens"], 100);
    assert!(body["usage"].get("tokens_estimated").is_none());
    assert_eq!(body["usage"]["estimated"], true);

    // The configured remap reached Jev: custom question and yes-criterion,
    // default no-criterion, the upstream model id, the query in the state.
    let received = upstream.received_requests().await.expect("recorded");
    let sent: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(sent["model"], "jev-latest");
    assert_eq!(sent["state"], json!({"query": "cited precedent"}));
    let q = &sent["questions"]["0"];
    assert_eq!(q["type"], "noul");
    assert_eq!(q["instructions"]["document"], "low #0.1");
    assert_eq!(
        q["instructions"]["question"],
        "Could `document` be the cited precedent?"
    );
    assert_eq!(q["criteria"]["true"], "States the cited rule.");
    assert_eq!(
        q["criteria"]["false"],
        lumen_providers::decisions::rerank::DEFAULT_CRITERIA_FALSE
    );
}

const UPSTREAM_OUT: &str = r#"{"model":"jev-1.13.0","answers":{"is_urgent":{"type":"noul","noul":0.95},"mood":{"type":"score","score":1.2,"legend":{"0":"calm","1":"angry"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.7,"future_answer_field":1}},"usage":{"input_tokens":318,"output_tokens":34},"request_id":"abc"}"#;

#[tokio::test]
async fn golden_passthrough_response_bytes() {
    let upstream = wiremock::MockServer::start().await;
    mount_answers(&upstream, UPSTREAM_OUT).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;
    let body = r#"{"model":"jev","state":"s","questions":{"is_urgent":{"type":"noul","instructions":"u?"},"mood":{"type":"score","instructions":"m?","criteria":["calm","angry"]}}}"#;
    let bytes = post_to(&base, "/v1/systemone", body)
        .await
        .bytes()
        .await
        .unwrap();
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../providers/tests/fixtures/decisions");
    if std::env::var_os("LUMEN_BLESS").is_some() {
        std::fs::write(dir.join("passthrough_response_in.json"), UPSTREAM_OUT).unwrap();
        std::fs::write(dir.join("passthrough_response_out.json"), &bytes).unwrap();
    } else {
        assert_eq!(
            bytes.as_ref(),
            std::fs::read(dir.join("passthrough_response_out.json")).unwrap()
        );
    }
}

// ---- /v1/decisions across vendors (ADR 016) ----------------------------------

/// An OpenAI-format request with a named predicate, a choice between a
/// string and a boolean, and a score: [`LUNA_OUT`] answers it.
const OPENAI_REQUEST: &str = r#"{"model":"luna","input":"x","questions":[{"type":"predicate","name":"u","instructions":"i"},{"type":"choice","instructions":"i","choices":[{"value":"a"},{"value":false}]},{"type":"score","instructions":"i","levels":[{"label":"lo"},{"label":"hi"}]}]}"#;

/// OpenAI's answer to [`OPENAI_REQUEST`]: the third question is refused.
const LUNA_OUT: &str = r#"{"model":"gpt-6-luna","answers":[{"type":"predicate","name":"u","probability":0.8},{"type":"choice","name":null,"choice":false,"probabilities":[{"value":false,"probability":0.9},{"value":"a","probability":0.1}],"confidence":0.9},{"type":"refusal","name":null}],"usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":0},"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":23}}"#;

/// An OpenAI-format request carrying an image, one predicate.
const IMAGE_REQUEST: &str = r#"{"model":"jev-pinned","input":[{"role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,AA"}]}],"questions":[{"type":"predicate","instructions":"Is it a cat?"}]}"#;

/// OpenAI's answer to [`IMAGE_REQUEST`].
const LUNA_IMAGE_OUT: &str = r#"{"model":"gpt-6-luna","answers":[{"type":"predicate","name":null,"probability":0.7}],"usage":{"input_tokens":90,"output_tokens":1,"total_tokens":91}}"#;

/// A TypeSafe-format request with a one-option choice (OpenAI rejects it).
const ONE_OPTION_REQUEST: &str = r#"{"model":"cross-luna-first","state":"s","questions":{"c":{"type":"choice","instructions":"i","criteria":{"only":null}}}}"#;

/// The TypeSafe-family answer to [`ONE_OPTION_REQUEST`].
const ONE_OPTION_ANSWERS: &str = r#"{"model":"decider-1","answers":{"c":{"type":"choice","choice":"only","probabilities":{"only":1.0},"confidence":1.0}},"usage":{"input_tokens":12,"output_tokens":1}}"#;

async fn mount_at(upstream: &MockServer, at: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(at.to_owned()))
        .respond_with(
            ResponseTemplate::new(status).set_body_raw(body.to_owned(), "application/json"),
        )
        .mount(upstream)
        .await;
}

fn model_used(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-lumen-model-used")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn scrape(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape")
        .text()
        .await
        .expect("text")
}

#[tokio::test]
async fn openai_format_to_openai_upstream_renders_openai_shape() {
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LUNA_OUT, "application/json"))
        .expect(1)
        .mount(&luna)
        .await;
    let other = MockServer::start().await;
    let base = spawn_upstreams(&Upstreams {
        luna: &luna.uri(),
        ..Upstreams::one(&other.uri())
    })
    .await;

    let resp = post(&base, OPENAI_REQUEST).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(model_used(&resp).as_deref(), Some("luna"));
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let body: Value = resp.json().await.expect("json");
    let answers = body["answers"].as_array().expect("answers is an array");
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0]["type"], "predicate");
    assert_eq!(answers[2]["type"], "refusal");
    assert_eq!(body["usage"]["input_tokens"], 20);
    assert_eq!(body["usage"]["total_tokens"], 23);

    // The upstream got the OpenAI wire with the upstream model id and the key.
    let received = luna.received_requests().await.expect("recorded");
    let sent: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(sent["model"], "gpt-6-luna");
    assert_eq!(sent["input"], "x");
    assert_eq!(sent["questions"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        received[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(other
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

#[tokio::test]
async fn typesafe_format_falls_back_across_vendors_both_directions() {
    // TypeSafe format: Perplexity is overloaded, Jev (TypeSafe) answers, and
    // the client receives Jev's bytes verbatim.
    let decider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(529))
        .expect(1..)
        .mount(&decider)
        .await;
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANSWERS, "application/json"))
        .expect(1)
        .mount(&jev)
        .await;
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LUNA_OUT, "application/json"))
        .expect(0)
        .mount(&luna)
        .await;
    let base = spawn_upstreams(&Upstreams {
        primary: &jev.uri(),
        fallback: &jev.uri(),
        luna: &luna.uri(),
        decider: &decider.uri(),
        circuit_failure_threshold: 5,
    })
    .await;

    let resp = post(
        &base,
        &REQUEST.replace(r#""model":"jev""#, r#""model":"cross""#),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(model_used(&resp).as_deref(), Some("jev"));
    assert_eq!(resp.text().await.expect("body"), ANSWERS);
    // Perplexity got the TypeSafe wire, at its own upstream id.
    let sent = decider.received_requests().await.expect("recorded");
    let first: Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(first["model"], "decider-1");
    assert!(first["questions"].is_object());
    decider.verify().await;
    jev.verify().await;
    luna.verify().await;

    // OpenAI format: Perplexity and Jev are overloaded, OpenAI answers, and
    // the client receives the OpenAI shape.
    let decider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(529))
        .expect(1..)
        .mount(&decider)
        .await;
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(529))
        .expect(1..)
        .mount(&jev)
        .await;
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LUNA_OUT, "application/json"))
        .expect(1)
        .mount(&luna)
        .await;
    let base = spawn_upstreams(&Upstreams {
        primary: &jev.uri(),
        fallback: &jev.uri(),
        luna: &luna.uri(),
        decider: &decider.uri(),
        circuit_failure_threshold: 5,
    })
    .await;

    let resp = post(
        &base,
        &OPENAI_REQUEST.replace(r#""model":"luna""#, r#""model":"cross""#),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(model_used(&resp).as_deref(), Some("luna"));
    assert_eq!(
        resp.headers()
            .get("x-lumen-route")
            .and_then(|v| v.to_str().ok())
            .map(|r| r.ends_with("luna")),
        Some(true)
    );
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["answers"].as_array().map(Vec::len), Some(3));
    assert_eq!(body["usage"]["total_tokens"], 23);
    // Jev got the OpenAI request translated to its TypeSafe wire.
    let sent = jev.received_requests().await.expect("recorded");
    let to_jev: Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(to_jev["model"], "jev-latest");
    assert!(to_jev["questions"].is_object());
    decider.verify().await;
    jev.verify().await;
    luna.verify().await;
}

#[tokio::test]
async fn image_request_skips_jev_and_lm_2003_when_nothing_is_left() {
    let typesafe = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANSWERS, "application/json"))
        .expect(0)
        .mount(&typesafe)
        .await;
    let decider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANSWERS, "application/json"))
        .expect(0)
        .mount(&decider)
        .await;
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LUNA_IMAGE_OUT, "application/json"))
        .expect(1)
        .mount(&luna)
        .await;
    let base = spawn_upstreams(&Upstreams {
        primary: &typesafe.uri(),
        fallback: &typesafe.uri(),
        luna: &luna.uri(),
        decider: &decider.uri(),
        circuit_failure_threshold: 5,
    })
    .await;

    // Both Jev targets are text-only: nothing is left, LM-2003 names the first.
    let resp = post(&base, IMAGE_REQUEST).await;
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-2003", "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("jev-pinned-leaf")),
        "{err}"
    );

    // `cross`: Perplexity and Jev are text-only (the config default), so
    // both are skipped and OpenAI answers with the image it received.
    let resp = post(
        &base,
        &IMAGE_REQUEST.replace(r#""model":"jev-pinned""#, r#""model":"cross""#),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(model_used(&resp).as_deref(), Some("luna"));
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["answers"][0]["probability"], 0.7);
    let sent = luna.received_requests().await.expect("recorded");
    let to_luna: Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(
        to_luna["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,AA"
    );
    typesafe.verify().await;
    decider.verify().await;
    luna.verify().await;
}

#[tokio::test]
async fn an_incompatible_skip_is_not_an_attempt() {
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LUNA_OUT, "application/json"))
        .expect(1)
        .mount(&luna)
        .await;
    let decider = MockServer::start().await;
    mount_at(&decider, "/v1/decisions", 200, ONE_OPTION_ANSWERS).await;
    let typesafe = MockServer::start().await;
    let (base, resilience) = spawn_with_resilience(&Upstreams {
        primary: &typesafe.uri(),
        fallback: &typesafe.uri(),
        luna: &luna.uri(),
        decider: &decider.uri(),
        // A single counted failure would open OpenAI's circuit.
        circuit_failure_threshold: 1,
    })
    .await;

    for _ in 0..3 {
        let resp = post(&base, ONE_OPTION_REQUEST).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(model_used(&resp).as_deref(), Some("decider"));
        assert_eq!(
            resp.headers()
                .get("x-lumen-route")
                .and_then(|v| v.to_str().ok())
                .map(|r| r.ends_with("decider")),
            Some(true)
        );
        assert_eq!(resp.text().await.expect("body"), ONE_OPTION_ANSWERS);
    }
    assert_eq!(
        decider.received_requests().await.expect("recorded").len(),
        3
    );
    assert!(typesafe
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());

    // Not a circuit failure: OpenAI's breaker is not open, in /metrics or in
    // the runtime, and a compatible request to `luna` still reaches it.
    let metrics = scrape(&base).await;
    assert!(
        !metrics
            .lines()
            .any(|l| l.starts_with("lumen_circuit_state{")
                && l.contains(r#"provider="openai""#)
                && !l.ends_with(" 0")),
        "{metrics}"
    );
    assert!(!resilience
        .breakers
        .snapshot()
        .iter()
        .any(|(provider, model, state)| provider == "openai"
            && model == "luna"
            && *state != lumen_router::circuit::CircuitState::Closed));
    // Not an attempt: the skipped target produced no token sample.
    assert!(
        !metrics
            .lines()
            .any(|l| l.starts_with("lumen_tokens_total{") && l.contains(r#"model="luna""#)),
        "{metrics}"
    );
    let resp = post(&base, OPENAI_REQUEST).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(model_used(&resp).as_deref(), Some("luna"));
    luna.verify().await;
}

#[tokio::test]
async fn systemone_alias_carries_deprecation_headers_and_is_counted() {
    let upstream = MockServer::start().await;
    mount_answers(&upstream, ANSWERS).await;
    let base = spawn(&upstream.uri(), &upstream.uri()).await;

    let resp = post_to(&base, "/v1/systemone", REQUEST).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["deprecation"], "@1791417600");
    assert_eq!(
        resp.headers()["link"],
        r#"</docs/decisions#migrating-from-v1systemone>; rel="deprecation""#
    );
    assert_eq!(resp.text().await.expect("body"), ANSWERS);

    let openai =
        r#"{"model":"jev","input":"x","questions":[{"type":"predicate","instructions":"i"}]}"#;
    let rejected = post_to(&base, "/v1/systemone", openai).await;
    assert_eq!(rejected.status(), 400);
    assert!(
        rejected.headers().contains_key("deprecation"),
        "errors carry the headers too"
    );
    assert!(rejected.headers().contains_key("link"));
    let err: Value = rejected.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-1001");

    // `/v1/decisions` carries no deprecation header and is not counted.
    let resp = post(&base, REQUEST).await;
    assert_eq!(resp.status(), 200);
    assert!(!resp.headers().contains_key("deprecation"));

    let metrics = scrape(&base).await;
    assert!(
        metrics.contains(r#"lumen_deprecated_requests_total{route="/v1/systemone"} 2"#),
        "{metrics}"
    );
}

#[tokio::test]
async fn refusals_are_counted() {
    let luna = MockServer::start().await;
    mount_at(&luna, "/v1/decisions", 200, LUNA_OUT).await;
    let other = MockServer::start().await;
    let base = spawn_upstreams(&Upstreams {
        luna: &luna.uri(),
        ..Upstreams::one(&other.uri())
    })
    .await;

    let resp = post(&base, OPENAI_REQUEST).await;
    assert_eq!(resp.status(), 200);
    let metrics = scrape(&base).await;
    assert!(
        metrics.contains(r#"lumen_decision_refusals_total{model="luna"} 1"#),
        "{metrics}"
    );
}

/// Spawn with virtual-key auth enabled (no key issued) and a small body
/// limit, around the standard config.
async fn spawn_auth_small_limit(upstream: &str, body_limit: usize) -> String {
    use lumen_auth::key::hash_key;
    use lumen_auth::state::AuthState;
    use lumen_auth::store::KeyStore;
    use lumen_server::auth::AuthRuntime;

    let config = config(&Upstreams::one(upstream));
    let registry = Arc::new(
        Registry::build(
            config.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let store = KeyStore::in_memory().await.expect("open store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store,
        admin_token_hash: hash_key(&"a".repeat(64)),
        master: None,
    });
    let state = common::base_state(registry)
        .with_resilience(Arc::new(ResilienceRuntime::from_config(&config, None)))
        .with_auth(runtime);
    common::spawn_state(state, body_limit).await
}

fn assert_deprecation_headers(resp: &reqwest::Response) {
    assert_eq!(resp.headers()["deprecation"], "@1791417600");
    assert_eq!(
        resp.headers()["link"],
        r#"</docs/decisions#migrating-from-v1systemone>; rel="deprecation""#
    );
}

#[tokio::test]
async fn systemone_rejections_before_the_handler_still_carry_deprecation_headers() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ANSWERS, "application/json"))
        .expect(0)
        .mount(&upstream)
        .await;
    let base = spawn_auth_small_limit(&upstream.uri(), 1024).await;

    // Auth enabled, no key: 401 LM-4004 from the auth layer.
    let resp = post_to(&base, "/v1/systemone", REQUEST).await;
    assert_eq!(resp.status(), 401);
    assert_deprecation_headers(&resp);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-4004");

    // Over the body limit: 413 LM-1002 from the body-limit layer.
    let big = format!(
        r#"{{"model":"jev","state":"{}","questions":{{"q":{{"type":"noul","instructions":"i"}}}}}}"#,
        "x".repeat(4096)
    );
    let resp = post_to(&base, "/v1/systemone", &big).await;
    assert_eq!(resp.status(), 413);
    assert_deprecation_headers(&resp);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["error"]["code"], "LM-1002");

    // Other routes are untouched: 401 without the deprecation headers.
    let resp = post(&base, REQUEST).await;
    assert_eq!(resp.status(), 401);
    assert!(!resp.headers().contains_key("deprecation"));
    assert!(!resp.headers().contains_key("link"));

    // Counted once per request, rejected or not.
    let metrics = scrape(&base).await;
    assert!(
        metrics.contains(r#"lumen_deprecated_requests_total{route="/v1/systemone"} 2"#),
        "{metrics}"
    );
    upstream.verify().await;
}
