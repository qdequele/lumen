//! ADR 014: Jev answering /v1/rerank through a virtual-model remap, with a
//! Cohere fallback, over the real HTTP stack.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

async fn spawn(cfg: &Config) -> String {
    let registry = Arc::new(
        Registry::build(
            cfg.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .unwrap(),
    );
    let state = common::base_state(registry)
        .with_pricing(CostTable::from_config(cfg))
        .with_resilience(Arc::new(ResilienceRuntime::from_config(cfg, None)));
    common::spawn_state(state, LIMIT).await
}

fn config(jev: &str, cohere: &str, remap: &str) -> Config {
    Config::load_text(
        &format!(
            r#"
            [resilience]
            retry_max_attempts = 1
            retry_base_ms = 10
            retry_max_ms = 20

            [[providers]]
            name = "typesafe"
            kind = "typesafe"
            base_url = "{jev}"
            api_key_env = "LUMEN_TEST_TYPESAFE_KEY_UNUSED"
            [[providers.models]]
            id = "jev"
            upstream_id = "jev-latest"
            capabilities = ["decisions"]
            cost_per_1m_input = 0.042

            [[providers]]
            name = "cohere"
            kind = "cohere"
            base_url = "{cohere}"
            api_key_env = "LUMEN_TEST_COHERE_KEY_UNUSED"
            [[providers.models]]
            id = "rerank-english"
            upstream_id = "rerank-v3.5"
            capabilities = ["rerank"]

            [[virtual_models]]
            id = "acme/rerank"
            capability = "rerank"
            strategy = "fallback"
            targets = [{{ model = "jev", remap = {remap} }}, {{ model = "rerank-english" }}]
            "#
        ),
        "test",
    )
    .unwrap()
}

/// Answers every question of a decisions request with `answer(id)`.
struct Jev(fn(&str) -> Value);

impl Respond for Jev {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let answers: serde_json::Map<String, Value> = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|id| (id.clone(), (self.0)(id)))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0", "answers": answers,
            "usage": { "input_tokens": 123, "output_tokens": 4 }
        }))
    }
}

async fn rerank(base: &str, docs: &[&str]) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "acme/rerank", "query": "q", "documents": docs }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn noul_remap_ranks_with_jev_and_reports_upstream_tokens() {
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(Jev(
            |id| json!({ "type": "noul", "noul": if id == "1" { 0.9 } else { 0.2 } }),
        ))
        .mount(&jev)
        .await;
    let cohere = MockServer::start().await;
    let base = spawn(&config(
        &jev.uri(),
        &cohere.uri(),
        r#"{ context = "support KB" }"#,
    ))
    .await;
    let resp = rerank(&base, &["a", "b"]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lumen-model-used"], "jev");
    assert_eq!(resp.headers()["x-lumen-route"], "acme/rerank>jev");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["results"][0]["index"], 1, "most relevant first");
    assert_eq!(body["usage"]["total_tokens"], 123);
    assert!(body["usage"].get("tokens_estimated").is_none());
    let sent: Value =
        serde_json::from_slice(&jev.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["state"]["context"], "support KB");
    assert_eq!(sent["model"], "jev-latest");
    assert!(cohere.received_requests().await.unwrap().is_empty());
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(
            r#"lumen_virtual_model_requests_total{model_used="jev",virtual_model="acme/rerank"} 1"#
        ),
        "{metrics}"
    );
}

#[tokio::test]
async fn score_composite_and_choice_strategies_end_to_end() {
    let cohere = MockServer::start().await;

    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(Jev(|_| json!({ "type": "score", "score": 2.0 })))
        .mount(&jev)
        .await;
    let base = spawn(&config(
        &jev.uri(),
        &cohere.uri(),
        r#"{ strategy = "score", levels = ["no", "partly", "yes"] }"#,
    ))
    .await;
    let body: Value = rerank(&base, &["a"]).await.json().await.unwrap();
    assert!((body["results"][0]["relevance_score"].as_f64().unwrap() - 1.0).abs() < 1e-6);

    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(Jev(
            |id| json!({ "type": "noul", "noul": if id.ends_with(".0") { 1.0 } else { 0.0 } }),
        ))
        .mount(&jev)
        .await;
    let base = spawn(&config(&jev.uri(), &cohere.uri(),
        r#"{ strategy = "composite", questions = [{ instructions = "on topic?", weight = 1.0 }, { instructions = "answers?", weight = 1.0 }] }"#)).await;
    let body: Value = rerank(&base, &["a"]).await.json().await.unwrap();
    assert!((body["results"][0]["relevance_score"].as_f64().unwrap() - 0.5).abs() < 1e-6);

    let jev = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/systemone"))
        .respond_with(Jev(|_| json!({ "type": "choice", "choice": "d0", "probabilities": { "d0": 0.7, "d1": 0.3 }, "confidence": 0.5 })))
        .mount(&jev).await;
    let base = spawn(&config(
        &jev.uri(),
        &cohere.uri(),
        r#"{ strategy = "choice" }"#,
    ))
    .await;
    let body: Value = rerank(&base, &["a", "b"]).await.json().await.unwrap();
    assert_eq!(body["results"][0]["index"], 0);
    assert_eq!(jev.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn jev_overload_falls_back_to_cohere() {
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(529))
        .mount(&jev)
        .await;
    let cohere = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 0, "relevance_score": 0.8 }],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(&cohere)
        .await;
    let base = spawn(&config(&jev.uri(), &cohere.uri(), "{}")).await;
    let resp = rerank(&base, &["a"]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lumen-model-used"], "rerank-english");
    assert_eq!(
        resp.headers()["x-lumen-route"],
        "acme/rerank>rerank-english"
    );
}

#[tokio::test]
async fn a_choice_over_255_documents_is_lm_1001_before_any_call() {
    let jev = MockServer::start().await;
    let cohere = MockServer::start().await;
    let base = spawn(&config(
        &jev.uri(),
        &cohere.uri(),
        r#"{ strategy = "choice" }"#,
    ))
    .await;
    let docs: Vec<String> = (0..256).map(|i| format!("doc {i}")).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let resp = rerank(&base, &refs).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");
    assert!(jev.received_requests().await.unwrap().is_empty());
    assert!(
        cohere.received_requests().await.unwrap().is_empty(),
        "a client error never fails over"
    );
}

#[tokio::test]
async fn remap_text_never_reaches_an_error_body() {
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&jev)
        .await;
    let cohere = MockServer::start().await;
    let base = spawn(&config(
        &jev.uri(),
        &cohere.uri(),
        r#"{ instructions = "SECRET-RELEVANCE-RULE" }"#,
    ))
    .await;
    let resp = rerank(&base, &["a"]).await;
    let text = resp.text().await.unwrap();
    assert!(!text.contains("SECRET-RELEVANCE-RULE"), "{text}");
}

/// A raw TCP upstream that accepts one connection, reads the request, never
/// answers, and reports when the gateway closes the connection.
async fn spawn_silent_upstream() -> (String, tokio::sync::oneshot::Receiver<()>) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 8192];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = tx.send(());
    });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn dropping_the_client_aborts_the_in_flight_jev_call() {
    let (jev, closed) = spawn_silent_upstream().await;
    let cohere = MockServer::start().await;
    let base = spawn(&config(&jev, &cohere.uri(), "{}")).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let _ = client
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "acme/rerank", "query": "q", "documents": ["a"] }))
        .send()
        .await; // times out on the client side and drops the connection
    let result = tokio::time::timeout(Duration::from_secs(3), closed).await;
    assert!(
        matches!(result, Ok(Ok(()))),
        "the Jev connection was not aborted after the client left"
    );
}

/// A rerank virtual model whose remap targets OpenAI's decision model, with
/// a Cohere fallback on `content_filter`.
fn openai_config(luna: &str, cohere: &str, remap: &str) -> Config {
    Config::load_text(
        &format!(
            r#"
            [resilience]
            retry_max_attempts = 1
            retry_base_ms = 10
            retry_max_ms = 20

            [[providers]]
            name = "openai"
            kind = "openai"
            base_url = "{luna}/v1"
            api_key_env = "LUMEN_TEST_OPENAI_KEY_UNUSED"
            [[providers.models]]
            id = "luna"
            upstream_id = "gpt-6-luna"
            capabilities = ["decisions"]

            [[providers]]
            name = "cohere"
            kind = "cohere"
            base_url = "{cohere}"
            api_key_env = "LUMEN_TEST_COHERE_KEY_UNUSED"
            [[providers.models]]
            id = "rerank-english"
            upstream_id = "rerank-v3.5"
            capabilities = ["rerank"]

            [[virtual_models]]
            id = "acme/rerank"
            capability = "rerank"
            strategy = "fallback"
            fallback_on = ["content_filter"]
            targets = [{{ model = "luna", remap = {remap} }}, {{ model = "rerank-english" }}]
            "#
        ),
        "test",
    )
    .unwrap()
}

async fn mount_luna(answers: Value) -> MockServer {
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna", "answers": answers,
            "usage": { "input_tokens": 50, "output_tokens": 2 }
        })))
        .mount(&luna)
        .await;
    luna
}

#[tokio::test]
async fn an_openai_remap_counts_refusals_and_never_returns_them() {
    let luna = mount_luna(json!([
        { "type": "refusal", "name": "0" },
        { "type": "predicate", "name": "1", "probability": 0.7 }
    ]))
    .await;
    let cohere = MockServer::start().await;
    let base = spawn(&openai_config(&luna.uri(), &cohere.uri(), "{}")).await;

    let resp = rerank(&base, &["a", "b"]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lumen-model-used"], "luna");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["results"][0]["index"], 1);
    assert_eq!(body["results"][1]["relevance_score"], 0.0);
    assert_eq!(body["usage"]["total_tokens"], 50);
    assert!(body["usage"].get("refusals").is_none(), "{body}");
    let sent: Value =
        serde_json::from_slice(&luna.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-6-luna");
    assert_eq!(sent["input"], r#"{"query":"q"}"#);
    assert!(cohere.received_requests().await.unwrap().is_empty());

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(r#"lumen_decision_refusals_total{model="luna"} 1"#),
        "{metrics}"
    );
}

#[tokio::test]
async fn a_refused_choice_falls_back_on_content_filter() {
    let luna = mount_luna(json!([{ "type": "refusal", "name": "rank" }])).await;
    let cohere = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 1, "relevance_score": 0.8 }, { "index": 0, "relevance_score": 0.1 }],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(&cohere)
        .await;
    let base = spawn(&openai_config(
        &luna.uri(),
        &cohere.uri(),
        r#"{ strategy = "choice" }"#,
    ))
    .await;

    let resp = rerank(&base, &["a", "b"]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lumen-model-used"], "rerank-english");
    assert_eq!(luna.received_requests().await.unwrap().len(), 1);

    // The refused attempt is counted against the model that refused, even
    // though another target served the request.
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(r#"lumen_decision_refusals_total{model="luna"} 1"#),
        "{metrics}"
    );
    assert!(
        !metrics.contains(r#"lumen_decision_refusals_total{model="rerank-english"}"#),
        "{metrics}"
    );
}

#[tokio::test]
async fn a_refused_choice_without_fallback_is_a_400_lm_2013() {
    let luna = mount_luna(json!([{ "type": "refusal", "name": "rank" }])).await;
    let cfg = Config::load_text(
        &format!(
            r#"
            [resilience]
            retry_max_attempts = 1

            [[providers]]
            name = "openai"
            kind = "openai"
            base_url = "{}/v1"
            api_key_env = "LUMEN_TEST_OPENAI_KEY_UNUSED"
            [[providers.models]]
            id = "luna"
            upstream_id = "gpt-6-luna"
            capabilities = ["decisions"]

            [[virtual_models]]
            id = "acme/rerank"
            capability = "rerank"
            strategy = "single"
            targets = [{{ model = "luna", remap = {{ strategy = "choice" }} }}]
            "#,
            luna.uri()
        ),
        "test",
    )
    .unwrap();
    let base = spawn(&cfg).await;

    let resp = rerank(&base, &["a", "b"]).await;
    // The upstream answered 200 with a refusal: the client gets a 4xx
    // content-filter error, never a 200 nor a 5xx.
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2013", "{body}");
    assert_eq!(luna.received_requests().await.unwrap().len(), 1);
}
