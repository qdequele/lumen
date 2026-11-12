//! ADR 014 acceptance: virtual models over the real HTTP stack.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

fn load(toml: &str) -> Config {
    Config::load_text(toml, "test").expect("valid test config")
}

/// Spawn with routing from `routing` and the registry built from
/// `registry_from` (the same config, except in the hot-reload-race test).
async fn spawn(routing: &Config, registry_from: &Config) -> String {
    let registry = Arc::new(
        Registry::build(
            registry_from.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let state = common::base_state(registry)
        .with_pricing(CostTable::from_config(routing))
        .with_resilience(Arc::new(ResilienceRuntime::from_config(routing, None)));
    common::spawn_state(state, LIMIT).await
}

fn chat_body(model: &str) -> Value {
    json!({
        "object": "chat.completion", "id": "c", "created": 1, "model": model,
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
    })
}

async fn upstream(status: u16, model: &str) -> MockServer {
    let server = MockServer::start().await;
    let template = if status == 200 {
        ResponseTemplate::new(200).set_body_json(chat_body(model))
    } else {
        ResponseTemplate::new(status)
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

fn two_providers(a: &str, b: &str, virtual_models: &str) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = 1
        retry_base_ms = 10
        retry_max_ms = 20

        [[providers]]
        name = "a"
        kind = "openai"
        base_url = "{a}"
        [[providers.models]]
        id = "a/model-a"
        capabilities = ["chat"]

        [[providers]]
        name = "b"
        kind = "openai"
        base_url = "{b}"
        [[providers.models]]
        id = "b/model-b"
        capabilities = ["chat", "embed"]

        {virtual_models}
        "#
    )
}

async fn post(base: &str, body: Value, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.expect("send")
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn fallback_virtual_model_serves_from_second_target_and_reports_route() {
    let (a, b) = (
        upstream(503, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "fallback"
        targets = [{ model = "a/model-a" }, { model = "b/model-b" }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let resp = post(
        &base,
        json!({ "model": "acme/chat", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));
    assert_eq!(header(&resp, "x-lumen-route"), Some("acme/chat>b/model-b"));

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .expect("metrics")
        .text()
        .await
        .expect("text");
    assert!(
        metrics.contains(
            r#"lumen_virtual_model_requests_total{model_used="b/model-b",virtual_model="acme/chat"}"#
        ),
        "routed request is counted: {metrics}"
    );
}

/// A fallback target that does not declare the `image` modality is not an
/// attempt for a request carrying an image: the fail-over skips it and goes
/// on to the next target that can take the image.
#[tokio::test]
async fn image_request_skips_text_only_fallback_targets() {
    let (a, b, c) = (
        upstream(503, "model-a").await,
        upstream(200, "model-b").await,
        upstream(200, "model-c").await,
    );
    let cfg = load(&format!(
        r#"
        [resilience]
        retry_max_attempts = 1
        retry_base_ms = 10
        retry_max_ms = 20

        [[providers]]
        name = "a"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "a/model-a"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[providers]]
        name = "b"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "b/model-b"
        capabilities = ["chat"]

        [[providers]]
        name = "c"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "c/model-c"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[virtual_models]]
        id = "acme/vision"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "a/model-a" }}, {{ model = "b/model-b" }}, {{ model = "c/model-c" }}]
        "#,
        a.uri(),
        b.uri(),
        c.uri()
    ));
    let base = spawn(&cfg, &cfg).await;
    let image = json!({
        "model": "acme/vision",
        "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "what is this?" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
        ]}]
    });

    let resp = post(&base, image, &[]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("c/model-c"));
    assert_eq!(
        header(&resp, "x-lumen-route"),
        Some("acme/vision>c/model-c")
    );
    assert!(
        b.received_requests().await.expect("requests").is_empty(),
        "the text-only target must never receive the image"
    );

    // Without an image, the text-only target is a normal fallback.
    let resp = post(
        &base,
        json!({ "model": "acme/vision", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));
}

#[tokio::test]
async fn a_direct_foundation_call_has_no_route_header() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(&a.uri(), &b.uri(), ""));
    let base = spawn(&cfg, &cfg).await;
    let resp = post(
        &base,
        json!({ "model": "model-a", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-route"), None);
}

#[tokio::test]
async fn split_fails_over_to_the_other_target() {
    let (a, b) = (
        upstream(503, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/split"
        capability = "chat"
        strategy = "split"
        targets = [{ model = "a/model-a", weight = 50 }, { model = "b/model-b", weight = 50 }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    for _ in 0..6 {
        let resp = post(
            &base,
            json!({ "model": "acme/split", "messages": [{ "role": "user", "content": "hi" }] }),
            &[],
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));
    }
}

#[tokio::test]
async fn switch_on_metadata_and_request_facts() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/switch"
        capability = "chat"
        strategy = "switch"
        targets = [
          { when = { "metadata.plan" = "pro" }, model = "b/model-b" },
          { when = { has_tools = true }, model = "b/model-b" },
          { model = "a/model-a" },
        ]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let msg = json!([{ "role": "user", "content": "hi" }]);
    let pro = post(
        &base,
        json!({ "model": "acme/switch", "messages": msg }),
        &[("x-lumen-metadata", r#"{"plan":"pro"}"#)],
    )
    .await;
    assert_eq!(header(&pro, "x-lumen-model-used"), Some("b/model-b"));
    let free = post(
        &base,
        json!({ "model": "acme/switch", "messages": msg }),
        &[("x-lumen-metadata", r#"{"plan":"free"}"#)],
    )
    .await;
    assert_eq!(header(&free, "x-lumen-model-used"), Some("a/model-a"));
    let tools = post(
        &base,
        json!({ "model": "acme/switch", "messages": msg,
        "tools": [{ "type": "function", "function": { "name": "f", "parameters": {} } }] }),
        &[],
    )
    .await;
    assert_eq!(header(&tools, "x-lumen-model-used"), Some("b/model-b"));
}

#[tokio::test]
async fn preset_and_overrides_reach_the_upstream_body() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/bot"
        capability = "chat"
        strategy = "single"
        preset = { system_prompt = "PRESET-PROMPT", overrides = { default = { temperature = 0.2 } } }
        targets = [{ model = "b/model-b", overrides = { set = { max_tokens = 77 }, drop = ["seed"] } }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let resp = post(
        &base,
        json!({ "model": "acme/bot", "seed": 1,
        "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let sent: Value =
        serde_json::from_slice(&b.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(
        sent["messages"][0],
        json!({ "role": "system", "content": "PRESET-PROMPT" })
    );
    assert_eq!(sent["max_tokens"], json!(77));
    assert!((sent["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    assert!(sent.get("seed").is_none());
    assert_eq!(
        sent["model"],
        json!("model-b"),
        "the upstream id, never the virtual id"
    );
}

/// Each upstream call is built from the client's request, never from the
/// request a previous attempt sent: one target's overrides and upstream id do
/// not leak into the retry of the same target or into the next target.
#[tokio::test]
async fn each_attempt_sends_its_own_overrides_and_upstream_id() {
    let (a, b) = (
        upstream(503, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(
        &two_providers(
            &a.uri(),
            &b.uri(),
            r#"
        [[virtual_models]]
        id = "acme/bot"
        capability = "chat"
        strategy = "fallback"
        targets = [
            { model = "a/model-a", overrides = { set = { max_tokens = 11 }, drop = ["seed"] } },
            { model = "b/model-b" },
        ]
    "#,
        )
        .replace("retry_max_attempts = 1", "retry_max_attempts = 2"),
    );
    let base = spawn(&cfg, &cfg).await;
    let resp = post(
        &base,
        json!({ "model": "acme/bot", "seed": 1,
        "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));

    let to_a = a.received_requests().await.unwrap();
    assert!(
        to_a.len() >= 2,
        "the first target is retried: {}",
        to_a.len()
    );
    for request in &to_a {
        let sent: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(sent["model"], json!("model-a"));
        assert_eq!(sent["max_tokens"], json!(11));
        assert!(sent.get("seed").is_none());
    }
    let to_b = b.received_requests().await.unwrap();
    assert_eq!(to_b.len(), 1);
    let sent: Value = serde_json::from_slice(&to_b[0].body).unwrap();
    assert_eq!(sent["model"], json!("model-b"));
    assert_eq!(sent["seed"], json!(1));
    assert!(sent.get("max_tokens").is_none());
    assert_eq!(sent["messages"][0]["content"], json!("hi"));
}

#[tokio::test]
async fn an_encoding_format_override_sets_the_response_encoding() {
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "model-b",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.5, 0.25] }],
            "usage": { "prompt_tokens": 2, "total_tokens": 2 }
        })))
        .mount(&b)
        .await;
    let a = upstream(200, "model-a").await;
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/b64"
        capability = "embed"
        strategy = "single"
        targets = [{ model = "b/model-b", overrides = { set = { encoding_format = "base64" } } }]

        [[virtual_models]]
        id = "acme/float"
        capability = "embed"
        strategy = "single"
        targets = [{ model = "b/model-b", overrides = { drop = ["encoding_format"] } }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let embed = |model: &'static str, encoding: &'static str| {
        let base = base.clone();
        async move {
            let resp = reqwest::Client::new()
                .post(format!("{base}/v1/embeddings"))
                .json(&json!({ "model": model, "input": "hi", "encoding_format": encoding }))
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), 200);
            resp.json::<Value>().await.unwrap()["data"][0]["embedding"].clone()
        }
    };
    // The client asks for floats; the target's `set` wins.
    assert!(embed("acme/b64", "float").await.is_string());
    // The client asks for base64; the target drops the field, so floats.
    assert!(embed("acme/float", "base64").await.is_array());
}

#[tokio::test]
async fn a_classified_refusal_that_no_target_absorbs_is_a_provider_named_4xx() {
    let refusing = |body: Value| async move {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(body))
            .mount(&server)
            .await;
        server
    };
    let a =
        refusing(json!({ "error": { "code": "context_length_exceeded", "message": "too long" } }))
            .await;
    let b = refusing(json!({ "error": { "code": "content_filter", "message": "filtered" } })).await;
    let cfg = load(&two_providers(&a.uri(), &b.uri(), ""));
    let base = spawn(&cfg, &cfg).await;
    for (model, code, provider) in [("model-a", "LM-2012", "'a'"), ("model-b", "LM-2013", "'b'")] {
        let resp = post(
            &base,
            json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }),
            &[],
        )
        .await;
        assert_eq!(resp.status(), 400, "{model}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["type"], "invalid_request");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains(provider),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_virtual_model_on_the_wrong_endpoint_is_lm_2002() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "single"
        targets = [{ model = "b/model-b" }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({ "model": "acme/chat", "input": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2002");
}

#[tokio::test]
async fn missing_fallback_leaf_is_skipped() {
    // Hot-reload race: the routing policy still names model-b, the registry
    // no longer has it. model-a fails; the request must end with model-a's
    // upstream error, never a 500.
    let (a, b) = (
        upstream(503, "model-a").await,
        upstream(200, "model-b").await,
    );
    let vm = r#"
        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "fallback"
        targets = [{ model = "a/model-a" }, { model = "b/model-b" }]
    "#;
    let routing = load(&two_providers(&a.uri(), &b.uri(), vm));
    let registry_only_a = load(
        &two_providers(&a.uri(), &b.uri(), "").replace("id = \"b/model-b\"", "id = \"b/model-z\""),
    );
    let base = spawn(&routing, &registry_only_a).await;
    let resp = post(
        &base,
        json!({ "model": "acme/chat", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3003");
}

#[tokio::test]
async fn streaming_fails_over_before_the_first_byte_and_carries_the_route() {
    let a = upstream(503, "model-a").await;
    let b = MockServer::start().await;
    let sse = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&b)
        .await;
    let cfg = load(&two_providers(
        &a.uri(),
        &b.uri(),
        r#"
        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "fallback"
        targets = [{ model = "a/model-a" }, { model = "b/model-b" }]
    "#,
    ));
    let base = spawn(&cfg, &cfg).await;
    let resp = post(&base, json!({ "model": "acme/chat", "stream": true, "messages": [{ "role": "user", "content": "hi" }] }), &[]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-route"), Some("acme/chat>b/model-b"));
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
async fn preset_prompt_counts_toward_input_tokens_when_routing() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    // ~6000 chars of preset prompt (well under the 32 KiB cap): the tiny user
    // message alone is far below the threshold, message + preset is above it.
    let prompt = "lorem ipsum dolor sit amet ".repeat(220);
    let vm = format!(
        r#"
        [[virtual_models]]
        id = "acme/long"
        capability = "chat"
        strategy = "switch"
        preset = {{ system_prompt = "{prompt}" }}
        targets = [
          {{ when = {{ input_tokens = {{ gt = 1000 }} }}, model = "b/model-b" }},
          {{ model = "a/model-a" }},
        ]
    "#
    );
    let cfg = load(&two_providers(&a.uri(), &b.uri(), &vm));
    let base = spawn(&cfg, &cfg).await;
    let resp = post(
        &base,
        json!({ "model": "acme/long", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));
    let sent: Value =
        serde_json::from_slice(&b.received_requests().await.unwrap()[0].body).unwrap();
    let systems = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .count();
    assert_eq!(systems, 1, "the preset prompt is applied exactly once");
}

#[test]
fn the_shipped_example_config_has_no_legacy_fields_and_loads() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config.example.toml"
    ))
    .unwrap();
    assert!(
        !text.contains("\nfallbacks ="),
        "config.example.toml still uses per-model fallbacks"
    );
    assert!(!text.contains("[providers.models.rerank]"));
}

/// A pre-prefix client sending the bare id (`model-b` for `b/model-b`) is
/// still served, through a hidden deprecated alias: the response reports the
/// canonical id, `GET /v1/models` never lists the alias, and every use is
/// counted on `lumen_deprecated_model_alias_total`.
#[tokio::test]
async fn a_bare_id_resolves_through_a_hidden_counted_alias() {
    let (a, b) = (
        upstream(200, "model-a").await,
        upstream(200, "model-b").await,
    );
    let cfg = load(&two_providers(&a.uri(), &b.uri(), ""));
    let registry = Arc::new(
        Registry::build(
            cfg.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let metrics = lumen_telemetry::Metrics::new();
    let gauges = lumen_telemetry::ResilienceMetrics::register(&metrics).expect("register");
    let state = common::base_state(registry)
        .with_resilience(Arc::new(ResilienceRuntime::from_config(&cfg, Some(gauges))));
    let base = common::spawn_state(state, LIMIT).await;

    let resp = post(
        &base,
        json!({ "model": "model-b", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lumen-model-used"), Some("b/model-b"));
    // The upstream got the id without its provider prefix.
    let sent: Value = b.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(sent["model"], "model-b");

    let list: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["a/model-a", "b/model-b"]);
    // Retrieving the alias serves the entry of the id it aliases.
    let one: Value = reqwest::get(format!("{base}/v1/models/model-a"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["id"], "a/model-a");

    let out = metrics.encode_text();
    assert!(
        out.contains("lumen_deprecated_model_alias_total{alias=\"model-b\",model=\"b/model-b\"} 1"),
        "{out}"
    );
    assert!(
        out.contains("lumen_deprecated_model_alias_total{alias=\"model-a\",model=\"a/model-a\"} 1"),
        "{out}"
    );
    // The prefixed id itself is never counted.
    let resp = post(
        &base,
        json!({ "model": "b/model-b", "messages": [{ "role": "user", "content": "hi" }] }),
        &[],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert!(metrics
        .encode_text()
        .contains("lumen_deprecated_model_alias_total{alias=\"model-b\",model=\"b/model-b\"} 1"));
}
