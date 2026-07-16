//! End-to-end tests for M5: virtual-key auth, hard budgets, quotas, token
//! accounting, cost counting, usage logging, metadata (ADR 002) and the
//! admin API. The upstream is wiremock; LUMEN sits in front with auth
//! enabled and an in-memory SQLite store.

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
use lumen_telemetry::{Metrics, TokenMetrics};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// The master key value (64 hex chars) used as the admin bearer token.
fn master() -> String {
    "a".repeat(64)
}

/// One registry over a single wiremock server: OpenAI (chat + embeddings),
/// Cohere (rerank) and TEI (keyless embeddings) - the paths never collide.
fn full_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![
        ProviderSpec {
            name: "openai".to_owned(),
            kind: ProviderKind::Openai,
            api_key: Some("sk-test-xxx".to_owned()),
            base_url: Some(upstream.to_owned()),
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![
                ModelSpec {
                    id: "gpt".to_owned(),
                    upstream_id: "gpt-4o-2024-08-06".to_owned(),
                    capabilities: vec![Capability::Chat],
                    modalities: vec!["text".to_owned()],
                },
                ModelSpec {
                    id: "embed-small".to_owned(),
                    upstream_id: "text-embedding-3-small".to_owned(),
                    capabilities: vec![Capability::Embed],
                    modalities: vec!["text".to_owned()],
                },
            ],
        },
        ProviderSpec {
            name: "cohere".to_owned(),
            kind: ProviderKind::Cohere,
            api_key: Some("sk-co-test".to_owned()),
            base_url: Some(upstream.to_owned()),
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "rerank-fast".to_owned(),
                upstream_id: "rerank-v3.5".to_owned(),
                capabilities: vec![Capability::Rerank],
                modalities: vec!["text".to_owned()],
            }],
        },
        ProviderSpec {
            name: "tei".to_owned(),
            kind: ProviderKind::Tei,
            api_key: None,
            base_url: Some(upstream.to_owned()),
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "tei-embed".to_owned(),
                upstream_id: "tei-model".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: vec!["text".to_owned()],
            }],
        },
    ];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

/// $1 per token / per search: budget arithmetic in the tests is 1:1.
fn dollar_pricing() -> CostTable {
    let toml = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        cost_per_1m_input = 1000000.0
        cost_per_1m_output = 1000000.0
        [[providers.models]]
        id = "embed-small"
        capabilities = ["embed"]
        cost_per_1m_input = 1000000.0

        [[providers]]
        name = "cohere"
        kind = "cohere"
        [[providers.models]]
        id = "rerank-fast"
        capabilities = ["rerank"]
        cost_per_1k_searches = 1000.0
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
    runtime: Arc<AuthRuntime>,
    client: reqwest::Client,
    /// The usage writer task, so tests can kill it (jammed-DB scenarios).
    writer: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn create_key(&self, budget: Option<f64>, rpm: Option<i64>, tpm: Option<i64>) -> String {
        let (plaintext, record) = self
            .store
            .create_key(NewKey {
                name: "test-key".to_owned(),
                budget_max: budget,
                rpm_limit: rpm,
                tpm_limit: tpm,
                expires_at: None,
            })
            .await
            .expect("create key");
        self.runtime
            .keys
            .upsert(hash_key(plaintext.reveal()), &record);
        plaintext.reveal().to_owned()
    }

    async fn metrics_text(&self) -> String {
        self.client
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .expect("scrape metrics")
            .text()
            .await
            .expect("metrics body")
    }

    /// Wait (bounded) until the usage log holds `expected` rows.
    async fn wait_usage_rows(&self, expected: i64) {
        for _ in 0..200 {
            if self.store.count_usage().await.expect("count") >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "expected {expected} usage rows, has {}",
            self.store.count_usage().await.expect("count")
        );
    }
}

/// Spawn a full auth-enabled gateway around `registry`.
async fn spawn_auth(registry: Arc<Registry>, metadata_labels: &[&str]) -> Harness {
    let store = KeyStore::in_memory().await.expect("open store");
    spawn_auth_with_store(registry, metadata_labels, store).await
}

/// Same, but around an existing store - the "restart" scenario.
async fn spawn_auth_with_store(
    registry: Arc<Registry>,
    metadata_labels: &[&str],
    store: KeyStore,
) -> Harness {
    let entries = store.load_auth_entries().await.expect("load entries");
    let keys = AuthState::load(entries);
    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let (logger, writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 64,
            batch_max: 500,
            flush_interval: Duration::from_millis(25),
        },
    );

    let metrics = Metrics::new();
    let labels: Vec<String> = metadata_labels.iter().map(|&l| l.to_owned()).collect();
    let tokens = TokenMetrics::register(&metrics, &labels).expect("register token metrics");
    let latency =
        lumen_telemetry::LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_pricing(dollar_pricing())
        .with_auth(Arc::clone(&runtime))
        .with_usage(logger);
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        store,
        runtime,
        client: reqwest::Client::new(),
        writer,
    }
}

// ---- Upstream fixtures ------------------------------------------------------

async fn mount_openai_embeddings(upstream: &MockServer, prompt_tokens: u32) {
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": prompt_tokens, "total_tokens": prompt_tokens }
        })))
        .mount(upstream)
        .await;
}

async fn mount_tei_embeddings(upstream: &MockServer) {
    // TEI's /embed is a bare vector array: NO usage anywhere (criterion 9).
    Mock::given(method("POST"))
        .and(path("/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([[0.1, 0.2, 0.3]])))
        .mount(upstream)
        .await;
}

async fn mount_openai_chat(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 }
        })))
        .mount(upstream)
        .await;
}

async fn mount_cohere_rerank(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 0, "relevance_score": 0.9 },
                { "index": 1, "relevance_score": 0.1 }
            ],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(upstream)
        .await;
}

fn embed_body() -> Value {
    // "abcd" = 4 bytes → exactly 1 estimated token → $1 at test pricing.
    json!({ "model": "embed-small", "input": "abcd" })
}

// ---- Authentication ---------------------------------------------------------

#[tokio::test]
async fn missing_or_invalid_key_is_401_fg4004_before_upstream() {
    let upstream = MockServer::start().await;
    // No mounts: zero upstream traffic expected.
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    // No Authorization header at all.
    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4004");

    // A made-up key.
    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth("fg-not-a-real-key")
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);

    assert!(upstream.received_requests().await.expect("reqs").is_empty());
}

#[tokio::test]
async fn health_and_metrics_stay_open_when_auth_is_enabled() {
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    let health = h
        .client
        .get(format!("{}/health", h.base))
        .send()
        .await
        .expect("health");
    assert_eq!(health.status(), 200);
    let metrics = h
        .client
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .expect("metrics");
    assert_eq!(metrics.status(), 200);
}

#[tokio::test]
async fn model_retrieve_sits_behind_virtual_key_auth() {
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    // No key: 401 with the standard auth envelope, like every /v1 route.
    let resp = h
        .client
        .get(format!("{}/v1/models/gpt", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4004");

    // A valid virtual key gets the model object.
    let key = h.create_key(None, None, None).await;
    let resp = h
        .client
        .get(format!("{}/v1/models/gpt", h.base))
        .bearer_auth(&key)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["id"], "gpt");
    assert_eq!(body["object"], "model");
}

// ---- Hard budgets (criteria 1 & 2) -----------------------------------------

#[tokio::test]
async fn exhausted_budget_is_402_fg4001_with_zero_upstream_calls() {
    // Criterion 2: the refusal happens BEFORE any upstream call.
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    // Budget covers nothing: the $1 estimate cannot be reserved.
    let key = h.create_key(Some(0.5), None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4001");

    // wiremock received NOTHING (criterion 2).
    assert!(upstream.received_requests().await.expect("reqs").is_empty());
}

#[tokio::test]
async fn race_50_concurrent_requests_on_budget_for_10_exactly_10_pass() {
    // Criterion 1, at the HTTP level: 50 concurrent $1 requests against a
    // $10 budget → exactly 10 × 200, 40 × 402, zero overrun.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(Some(10.0), None, None).await;

    let mut futures = Vec::new();
    for _ in 0..50 {
        let client = h.client.clone();
        let url = format!("{}/v1/embeddings", h.base);
        let key = key.clone();
        futures.push(tokio::spawn(async move {
            client
                .post(url)
                .bearer_auth(key)
                .json(&embed_body())
                .send()
                .await
                .expect("send")
                .status()
                .as_u16()
        }));
    }
    let mut ok = 0;
    let mut rejected = 0;
    for f in futures {
        match f.await.expect("task") {
            200 => ok += 1,
            402 => rejected += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(ok, 10, "exactly the budget-covered requests pass");
    assert_eq!(rejected, 40);
    // Exactly 10 calls reached the provider.
    assert_eq!(upstream.received_requests().await.expect("reqs").len(), 10);
}

#[tokio::test]
async fn restart_reloads_flushed_budgets_and_an_exhausted_key_stays_exhausted() {
    // Criterion 6.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(Some(1.0), None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    // Simulate the periodic flush, then a restart over the same database.
    let dirty = h.runtime.keys.drain_dirty();
    assert!(!dirty.is_empty(), "spend must be flushable");
    h.store.persist_budgets(&dirty).await.expect("flush");

    let restarted =
        spawn_auth_with_store(full_registry(&upstream.uri()), &[], h.store.clone()).await;
    let resp = restarted
        .client
        .post(format!("{}/v1/embeddings", restarted.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 402, "reloaded budget stays exhausted");
}

// ---- Quotas ------------------------------------------------------------------

#[tokio::test]
async fn rpm_quota_is_429_fg4002_with_retry_after() {
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, Some(1), None).await;

    let url = format!("{}/v1/embeddings", h.base);
    let first = h
        .client
        .post(&url)
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(first.status(), 200);

    let second = h
        .client
        .post(&url)
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(second.status(), 429);
    assert!(second.headers().contains_key("retry-after"));
    let body: Value = second.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4002");
}

#[tokio::test]
async fn tpm_quota_is_429_fg4003() {
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    // 1 token per minute: the first request ("abcd" ≈ 1 token) uses it all.
    let key = h.create_key(None, None, Some(1)).await;

    let url = format!("{}/v1/embeddings", h.base);
    assert_eq!(
        h.client
            .post(&url)
            .bearer_auth(&key)
            .json(&embed_body())
            .send()
            .await
            .expect("send")
            .status(),
        200
    );
    let second = h
        .client
        .post(&url)
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(second.status(), 429);
    let body: Value = second.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4003");
}

// ---- Usage logging (criteria 3 & 4) -----------------------------------------

#[tokio::test]
async fn dead_usage_writer_never_blocks_requests_and_drops_are_counted() {
    // Criteria 3 + 4 (HTTP level): with the writer killed - the extreme
    // stand-in for a locked/unavailable DB - every log entry is dropped and
    // counted, and requests keep flowing at full speed. The full-but-alive
    // channel variant is covered deterministically by the auth crate's
    // `full_channel_drops_instead_of_blocking` unit test.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    h.writer.abort(); // nobody drains the channel any more
    let key = h.create_key(None, None, None).await;

    let url = format!("{}/v1/embeddings", h.base);
    let started = std::time::Instant::now();
    for _ in 0..70 {
        let resp = h
            .client
            .post(&url)
            .bearer_auth(&key)
            .json(&embed_body())
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200, "request path must not block on logging");
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "requests must not be slowed by the dead writer"
    );

    let metrics = h.metrics_text().await;
    assert!(
        metrics.contains("lumen_usage_log_dropped_total 70"),
        "every entry dropped AND counted, metrics:\n{metrics}"
    );
}

#[tokio::test]
async fn successful_requests_land_in_the_usage_log_with_cost() {
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 3).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(Some(100.0), None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    // model, capability, tokens_in=3 (upstream), cost $3, status 200.
    assert!(dump.contains("'embed-small'"), "dump:\n{dump}");
    assert!(dump.contains("'embed'"));
    assert!(dump.contains("|3|0|"), "tokens_in=3, tokens_out=0:\n{dump}");
    assert!(dump.contains("3.0"), "cost at $1/token:\n{dump}");
}

#[tokio::test]
async fn rejected_requests_land_in_the_usage_log_with_the_rejection_status() {
    // M5 point 3: a request refused at admission (here 402, budget) still
    // produces a status-only usage row - zero tokens, zero cost - so per-key
    // rejection analytics work. No upstream call happens.
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    // Budget covers nothing: the $1 estimate cannot be reserved → 402.
    let key = h.create_key(Some(0.5), None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 402);

    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    // The refused capability/model, status 402, and zero tokens are recorded.
    assert!(dump.contains("'embed-small'"), "dump:\n{dump}");
    assert!(dump.contains("'embed'"), "dump:\n{dump}");
    assert!(dump.contains("|402|"), "status 402 recorded:\n{dump}");
    assert!(
        dump.contains("|0|0|"),
        "zero tokens on a rejection:\n{dump}"
    );

    // Still no upstream traffic (the refusal is before any provider call).
    assert!(upstream.received_requests().await.expect("reqs").is_empty());
}

#[tokio::test]
async fn a_quota_rejection_is_also_logged() {
    // M5 point 3, the 429 variant: after the RPM cap is hit, the refused
    // request produces its own status-only row (status 429) alongside the
    // successful one.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, Some(1), None).await;

    let url = format!("{}/v1/embeddings", h.base);
    let first = h
        .client
        .post(&url)
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(first.status(), 200);
    let second = h
        .client
        .post(&url)
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(second.status(), 429);

    // Two rows: the success (200) and the rejection (429).
    h.wait_usage_rows(2).await;
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(dump.contains("|429|"), "429 rejection recorded:\n{dump}");
    assert!(dump.contains("|200|"), "200 success recorded:\n{dump}");
}

#[tokio::test]
async fn metadata_numeric_and_bool_values_are_stored_typed() {
    // M5 point 4: the usage-log metadata JSON keeps original value types, so
    // numeric/boolean filtering (SQLite json_extract) is possible - numbers
    // and bools are NOT stringified.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .header(
            "x-lumen-metadata",
            r#"{"batch":42,"canary":true,"team":"search"}"#,
        )
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(
        dump.contains(r#""batch":42"#),
        "number stays typed:\n{dump}"
    );
    assert!(
        dump.contains(r#""canary":true"#),
        "bool stays typed:\n{dump}"
    );
    assert!(
        dump.contains(r#""team":"search""#),
        "string stays quoted:\n{dump}"
    );
    assert!(
        !dump.contains(r#""batch":"42""#),
        "number must not be stringified:\n{dump}"
    );
}

// ---- Token accounting (criteria 9, 10, 11) ----------------------------------

#[tokio::test]
async fn tei_embeddings_without_upstream_usage_are_estimated_never_zero() {
    // Criterion 9.
    let upstream = MockServer::start().await;
    mount_tei_embeddings(&upstream).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "tei-embed", "input": "twelve bytes" }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    // The response surfaces the (flagged) estimate - never a silent zero.
    assert!(body["usage"]["prompt_tokens"].as_u64().expect("tokens") > 0);
    assert_eq!(body["usage"]["estimated"], true);

    let metrics = h.metrics_text().await;
    assert!(metrics.contains(r#"estimated="true""#), "{metrics}");
    assert!(metrics.contains(r#"model="tei-embed""#));
    // The dedicated estimation counter moved too.
    assert!(!metrics.contains("lumen_tokens_estimated_total 0"));

    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    // estimated=1 in the log; tokens_in = ceil(12/4) = 3.
    assert!(dump.contains("|3|0|"), "estimated 3 input tokens:\n{dump}");
}

#[tokio::test]
async fn openai_embeddings_use_upstream_usage_unestimated() {
    // Criterion 10.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 7).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["usage"]["prompt_tokens"], 7);
    assert!(body["usage"].get("estimated").is_none());

    let metrics = h.metrics_text().await;
    assert!(metrics.contains(r#"estimated="false""#), "{metrics}");
    assert!(metrics.contains("lumen_tokens_estimated_total 0"));
}

#[tokio::test]
async fn every_capability_feeds_the_token_counters_with_its_own_labels() {
    // Criterion 11: chat + embed + rerank each increment lumen_tokens_total
    // with the right capability/direction; rerank also counts search units.
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    mount_openai_embeddings(&upstream, 3).await;
    mount_cohere_rerank(&upstream).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(None, None, None).await;

    let chat = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&key)
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 50
        }))
        .send()
        .await
        .expect("chat");
    assert_eq!(chat.status(), 200);

    let embed = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&embed_body())
        .send()
        .await
        .expect("embed");
    assert_eq!(embed.status(), 200);

    let rerank = h
        .client
        .post(format!("{}/v1/rerank", h.base))
        .bearer_auth(&key)
        .json(&json!({
            "model": "rerank-fast",
            "query": "q",
            "documents": ["a", "b"]
        }))
        .send()
        .await
        .expect("rerank");
    assert_eq!(rerank.status(), 200);

    let metrics = h.metrics_text().await;
    for needle in [
        r#"capability="chat""#,
        r#"capability="embed""#,
        r#"capability="rerank""#,
        r#"direction="input""#,
        r#"direction="output""#,
        "lumen_rerank_search_units_total",
        r#"model="rerank-fast""#,
    ] {
        assert!(metrics.contains(needle), "missing {needle} in:\n{metrics}");
    }
}

#[tokio::test]
async fn streaming_chat_reads_usage_from_the_final_chunk() {
    // The stream sniffer extracts upstream usage (5.4b) and the usage log
    // records it unestimated.
    let upstream = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
        "\"model\":\"gpt-4o-2024-08-06\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
        "\"model\":\"gpt-4o-2024-08-06\",\"choices\":[],",
        "\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":34,\"total_tokens\":46}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;
    let key = h.create_key(Some(100.0), None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&key)
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 40,
            "stream": true
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.expect("stream body");
    assert!(text.contains("data: [DONE]"));

    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(dump.contains("|12|34|"), "upstream 12 in / 34 out:\n{dump}");

    let metrics = h.metrics_text().await;
    assert!(metrics.contains(r#"estimated="false""#), "{metrics}");
}

// ---- Metadata, ADR 002 (criteria 7 & 8) --------------------------------------

#[tokio::test]
async fn metadata_lands_in_usage_log_and_only_allowlisted_keys_become_labels() {
    // Criterion 7.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &["team"]).await;
    let key = h.create_key(None, None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .header("x-lumen-metadata", r#"{"team":"search","user_dim":"u-42"}"#)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    // Full object in the usage log.
    h.wait_usage_rows(1).await;
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(dump.contains(r#""team":"search""#), "{dump}");
    assert!(dump.contains(r#""user_dim":"u-42""#), "{dump}");

    // Prometheus: the allowlisted key becomes a label; the other key creates
    // NO time series (its value appears nowhere in the exposition).
    let metrics = h.metrics_text().await;
    assert!(metrics.contains(r#"team="search""#), "{metrics}");
    assert!(!metrics.contains("user_dim"), "{metrics}");
    assert!(!metrics.contains("u-42"), "{metrics}");
}

#[tokio::test]
async fn malformed_metadata_never_fails_the_request() {
    // Criterion 8.
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &["team"]).await;
    let key = h.create_key(None, None, None).await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .header("x-lumen-metadata", "definitely{not json")
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200, "the request must succeed anyway");

    let metrics = h.metrics_text().await;
    assert!(
        metrics.contains("lumen_metadata_rejected_total 1"),
        "{metrics}"
    );
    // Nothing leaked into labels.
    assert!(metrics.contains(r#"team="""#), "{metrics}");
}

// ---- Admin API (§5.5, criterion 5) -------------------------------------------

#[tokio::test]
async fn admin_requires_the_master_key() {
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    let no_auth = h
        .client
        .get(format!("{}/admin/keys", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(no_auth.status(), 401);

    let wrong = h
        .client
        .get(format!("{}/admin/keys", h.base))
        .bearer_auth("b".repeat(64))
        .send()
        .await
        .expect("send");
    assert_eq!(wrong.status(), 401);

    // A virtual key is NOT an admin key.
    let vkey = h.create_key(None, None, None).await;
    let with_vkey = h
        .client
        .get(format!("{}/admin/keys", h.base))
        .bearer_auth(&vkey)
        .send()
        .await
        .expect("send");
    assert_eq!(with_vkey.status(), 401);
}

#[tokio::test]
async fn admin_lifecycle_create_use_patch_disable() {
    let upstream = MockServer::start().await;
    mount_openai_embeddings(&upstream, 1).await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    // Create through the API: the ONLY moment the plaintext is visible.
    let created = h
        .client
        .post(format!("{}/admin/keys", h.base))
        .bearer_auth(master())
        .json(&json!({ "name": "team-search", "budget_max": 5.0 }))
        .send()
        .await
        .expect("create");
    assert_eq!(created.status(), 201);
    let body: Value = created.json().await.expect("json");
    let plaintext = body["key"].as_str().expect("key").to_owned();
    let id = body["id"].as_str().expect("id").to_owned();
    assert!(plaintext.starts_with("fg-"));
    assert_eq!(body["budget_max"], 5.0);

    // Criterion 5 (DB half): the plaintext exists nowhere at rest.
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(
        !dump.contains(&plaintext),
        "plaintext key stored somewhere!"
    );

    // The key is usable immediately, without a restart.
    let use_it = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&plaintext)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(use_it.status(), 200);

    // List shows the record but never key material.
    let list = h
        .client
        .get(format!("{}/admin/keys", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("list");
    let listed: Value = list.json().await.expect("json");
    assert_eq!(listed[0]["name"], "team-search");
    assert!(!listed.to_string().contains(&plaintext));

    // Disable via PATCH → refused on the very next call.
    let patched = h
        .client
        .patch(format!("{}/admin/keys/{id}", h.base))
        .bearer_auth(master())
        .json(&json!({ "disabled": true }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patched.status(), 200);
    let denied = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&plaintext)
        .json(&embed_body())
        .send()
        .await
        .expect("send");
    assert_eq!(denied.status(), 401);
}

#[tokio::test]
async fn stored_provider_keys_are_encrypted_at_rest() {
    let upstream = MockServer::start().await;
    let h = spawn_auth(full_registry(&upstream.uri()), &[]).await;

    let put = h
        .client
        .put(format!("{}/admin/provider-keys/openai", h.base))
        .bearer_auth(master())
        .json(&json!({ "key": "sk-live-super-secret" }))
        .send()
        .await
        .expect("put");
    assert_eq!(put.status(), 204);

    let dump = h.store.debug_dump().await.expect("dump");
    assert!(!dump.contains("sk-live-super-secret"), "{dump}");

    // Round-trips with the master key (what boot does).
    let master_key = MasterKey::from_env_value(&master()).expect("master");
    let loaded = h
        .store
        .load_provider_key("openai", &master_key)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded, "sk-live-super-secret");
}

#[tokio::test]
async fn admin_routes_do_not_exist_when_auth_is_disabled() {
    let base = common::spawn().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/keys"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}
