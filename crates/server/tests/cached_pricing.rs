//! Cached-input pricing (ADR 003 amendment): the cache-read and
//! cache-creation parts of a chat call's prompt tokens are charged at the
//! model's `cost_per_1m_cached_input` / `cost_per_1m_cache_write` rates, in
//! both the non-streaming and the streaming settlement. Only the cost moves:
//! the token counts in `usage_log` stay what the upstream reported. A model
//! with no cache rate prices exactly as before.

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewKey};
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::auth::AuthRuntime;
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use sqlx::Row;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

fn master() -> String {
    "a".repeat(64)
}

fn chat_model(id: &str) -> ModelSpec {
    ModelSpec {
        id: id.to_owned(),
        upstream_id: id.split('/').next_back().unwrap_or(id).to_owned(),
        capabilities: vec![Capability::Chat],
        modalities: vec!["text".to_owned()],
        release_date: None,
    }
}

fn provider(name: &str, kind: ProviderKind, upstream: &str, models: &[&str]) -> ProviderSpec {
    ProviderSpec {
        name: name.to_owned(),
        kind,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: models.iter().map(|id| chat_model(id)).collect(),
        decisions_path: None,
        forward_unknown_fields: None,
    }
}

/// OpenAI (`/chat/completions`) and Anthropic (`/v1/messages`) over one
/// wiremock server.
fn registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![
        provider(
            "openai",
            ProviderKind::Openai,
            upstream,
            &["openai/gpt", "openai/flat"],
        ),
        provider(
            "anthropic",
            ProviderKind::Anthropic,
            upstream,
            &["anthropic/claude"],
        ),
    ];
    Arc::new(
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    )
}

/// $1 per input and output token everywhere, so costs read as token counts.
/// `openai/gpt` reads its cache at $0.25 per token; `anthropic/claude` reads
/// at $0.10 and writes at $1.25; `openai/flat` sets no cache rate.
fn pricing() -> CostTable {
    let toml = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "openai/gpt"
        capabilities = ["chat"]
        cost_per_1m_input = 1000000.0
        cost_per_1m_output = 1000000.0
        cost_per_1m_cached_input = 250000.0
        [[providers.models]]
        id = "openai/flat"
        capabilities = ["chat"]
        cost_per_1m_input = 1000000.0
        cost_per_1m_output = 1000000.0

        [[providers]]
        name = "anthropic"
        kind = "anthropic"
        [[providers.models]]
        id = "anthropic/claude"
        capabilities = ["chat"]
        cost_per_1m_input = 1000000.0
        cost_per_1m_output = 1000000.0
        cost_per_1m_cached_input = 100000.0
        cost_per_1m_cache_write = 1250000.0
    "#;
    let config: Config = Figment::new()
        .merge(Toml::string(toml))
        .extract()
        .expect("valid pricing config");
    CostTable::from_config(&config)
}

struct Harness {
    base: String,
    store: KeyStore,
    key: String,
    client: reqwest::Client,
}

/// One `usage_log` row, the columns this file cares about.
#[derive(Debug, PartialEq)]
struct Row5 {
    tokens_in: i64,
    tokens_out: i64,
    cached: Option<i64>,
    cache_write: Option<i64>,
    cost: f64,
}

impl Harness {
    async fn chat(&self, model: &str, stream: bool) -> reqwest::Response {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(&json!({
                "model": model,
                "messages": [{ "role": "user", "content": "hi" }],
                "max_tokens": 40,
                "stream": stream
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200, "{model}");
        resp
    }

    /// The single usage row, once the writer has flushed it.
    async fn usage_row(&self) -> Row5 {
        for _ in 0..200 {
            if self.store.count_usage().await.expect("count") >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let row = sqlx::query(
            "SELECT tokens_in, tokens_out, cached_tokens, cache_write_tokens, cost FROM usage_log",
        )
        .fetch_one(self.store.pool())
        .await
        .expect("one usage row");
        Row5 {
            tokens_in: row.get("tokens_in"),
            tokens_out: row.get("tokens_out"),
            cached: row.get("cached_tokens"),
            cache_write: row.get("cache_write_tokens"),
            cost: row.get("cost"),
        }
    }
}

async fn spawn(upstream: &MockServer) -> Harness {
    let store = KeyStore::in_memory().await.expect("open store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let (plaintext, record) = store
        .create_key(NewKey {
            name: "k".to_owned(),
            budget_max: Some(1_000_000.0),
            ..NewKey::default()
        })
        .await
        .expect("create key");
    runtime.keys.upsert(hash_key(plaintext.reveal()), &record);
    let (logger, _writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 64,
            batch_max: 500,
            flush_interval: Duration::from_millis(25),
        },
    );
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("latency metrics");
    let state = AppState::new(metrics, registry(&upstream.uri()), tokens, latency)
        .with_pricing(pricing())
        .with_auth(runtime)
        .with_usage(logger);
    Harness {
        base: common::spawn_state(state, LIMIT).await,
        store,
        key: plaintext.reveal().to_owned(),
        client: reqwest::Client::new(),
    }
}

/// OpenAI usage: 100 prompt tokens of which 60 were read from cache, 10 out.
fn openai_usage() -> Value {
    json!({
        "prompt_tokens": 100,
        "completion_tokens": 10,
        "total_tokens": 110,
        "prompt_tokens_details": { "cached_tokens": 60 }
    })
}

async fn mount_openai(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello" },
                "finish_reason": "stop"
            }],
            "usage": openai_usage()
        })))
        .mount(upstream)
        .await;
}

async fn mount_openai_stream(upstream: &MockServer) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt",
            "choices": [{ "index": 0, "delta": { "content": "hi" } }]
        }),
        json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt",
            "choices": [],
            "usage": openai_usage()
        }),
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(upstream)
        .await;
}

/// 40 uncached at $1 + 60 cached at $0.25 + 10 out at $1.
const OPENAI_COST: f64 = 40.0 + 15.0 + 10.0;

#[tokio::test]
async fn non_streaming_cached_input_is_charged_at_the_cached_rate() {
    let upstream = MockServer::start().await;
    mount_openai(&upstream).await;
    let h = spawn(&upstream).await;

    let resp = h.chat("openai/gpt", false).await;
    let header = resp
        .headers()
        .get("x-lumen-cost-micro-usd")
        .expect("cost header")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert_eq!(header, "65000000", "the header carries the settled cost");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["usage"]["prompt_tokens"], 100, "counts untouched");
    assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 60);

    assert_eq!(
        h.usage_row().await,
        Row5 {
            tokens_in: 100,
            tokens_out: 10,
            cached: Some(60),
            cache_write: None,
            cost: OPENAI_COST,
        }
    );
}

#[tokio::test]
async fn streaming_cached_input_is_charged_at_the_cached_rate() {
    let upstream = MockServer::start().await;
    mount_openai_stream(&upstream).await;
    let h = spawn(&upstream).await;

    let text = h.chat("openai/gpt", true).await.text().await.expect("body");
    assert!(text.contains("data: [DONE]"), "{text}");

    assert_eq!(
        h.usage_row().await,
        Row5 {
            tokens_in: 100,
            tokens_out: 10,
            cached: Some(60),
            cache_write: None,
            cost: OPENAI_COST,
        }
    );
}

#[tokio::test]
async fn a_model_without_cache_rates_prices_as_before() {
    let upstream = MockServer::start().await;
    mount_openai(&upstream).await;
    let h = spawn(&upstream).await;

    h.chat("openai/flat", false).await;
    let row = h.usage_row().await;
    assert_eq!(row.cached, Some(60), "the breakdown is still recorded");
    assert!((row.cost - 110.0).abs() < 1e-9, "full input rate: {row:?}");
}

/// Anthropic: 30 uncached + 50 cache read + 20 cache creation = 100 prompt
/// tokens in OpenAI terms, 10 out.
/// 30 at $1 + 50 at $0.10 + 20 at $1.25 + 10 out at $1.
const ANTHROPIC_COST: f64 = 30.0 + 5.0 + 25.0 + 10.0;

#[tokio::test]
async fn anthropic_cache_read_and_creation_have_their_own_rates() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude",
            "content": [{ "type": "text", "text": "bonjour" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 30,
                "cache_read_input_tokens": 50,
                "cache_creation_input_tokens": 20,
                "output_tokens": 10
            }
        })))
        .mount(&upstream)
        .await;
    let h = spawn(&upstream).await;

    h.chat("anthropic/claude", false).await;
    let row = h.usage_row().await;
    assert_eq!(
        (row.tokens_in, row.tokens_out, row.cached, row.cache_write),
        (100, 10, Some(50), Some(20)),
        "{row:?}"
    );
    assert!((row.cost - ANTHROPIC_COST).abs() < 1e-9, "{row:?}");
}

#[tokio::test]
async fn anthropic_streaming_settles_at_the_same_cost() {
    let upstream = MockServer::start().await;
    let events = [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_s", "model": "claude",
                    "usage": {
                        "input_tokens": 30,
                        "cache_read_input_tokens": 50,
                        "cache_creation_input_tokens": 20
                    }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "bonjour" }
            }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 10 }
            }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ];
    let mut body = String::new();
    for (name, data) in &events {
        body.push_str("event: ");
        body.push_str(name);
        body.push_str("\ndata: ");
        body.push_str(&data.to_string());
        body.push_str("\n\n");
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&upstream)
        .await;
    let h = spawn(&upstream).await;

    let text = h
        .chat("anthropic/claude", true)
        .await
        .text()
        .await
        .expect("body");
    assert!(text.contains("data: [DONE]"), "{text}");

    let row = h.usage_row().await;
    assert_eq!(
        (row.tokens_in, row.tokens_out, row.cached, row.cache_write),
        (100, 10, Some(50), Some(20)),
        "{row:?}"
    );
    assert!((row.cost - ANTHROPIC_COST).abs() < 1e-9, "{row:?}");
}
