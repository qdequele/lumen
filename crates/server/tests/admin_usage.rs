//! Integration tests for `GET /admin/usage` (issue #64): master-key gating,
//! filter behavior, aggregation correctness over seeded rows, bounded result
//! size and the empty-result shape.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewKey, UsageRecord};
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::auth::AuthRuntime;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// The master key value (64 hex chars) used as the admin bearer token.
fn master() -> String {
    "a".repeat(64)
}

struct Harness {
    base: String,
    store: KeyStore,
    runtime: Arc<AuthRuntime>,
    client: reqwest::Client,
}

impl Harness {
    /// GET `/admin/usage` with the master key and the given query string.
    async fn usage(&self, query: &str) -> reqwest::Response {
        self.client
            .get(format!("{}/admin/usage{query}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("send")
    }

    async fn create_key(&self) -> String {
        let (plaintext, record) = self
            .store
            .create_key(NewKey {
                name: "test-key".to_owned(),
                budget_max: None,
                rpm_limit: None,
                tpm_limit: None,
                expires_at: None,
            })
            .await
            .expect("create key");
        self.runtime
            .keys
            .upsert(hash_key(plaintext.reveal()), &record);
        plaintext.reveal().to_owned()
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

/// Spawn an auth-enabled gateway around `registry`.
async fn spawn_admin(registry: Arc<Registry>) -> Harness {
    let store = KeyStore::in_memory().await.expect("open store");
    let entries = store.load_auth_entries().await.expect("load entries");
    let keys = AuthState::load(entries);
    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let (logger, _writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 64,
            batch_max: 500,
            flush_interval: Duration::from_millis(25),
        },
    );

    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_auth(Arc::clone(&runtime))
        .with_usage(logger);
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        store,
        runtime,
        client: reqwest::Client::new(),
    }
}

/// One usage row; the builder-style helpers below tweak single dimensions.
fn row(ts: i64) -> UsageRecord {
    UsageRecord {
        key_id: Some("key-a".to_owned()),
        model: "gpt".to_owned(),
        model_used: "gpt".to_owned(),
        provider: "openai".to_owned(),
        capability: "chat".to_owned(),
        tokens_in: 10,
        tokens_out: 20,
        search_units: None,
        media_count: 0,
        media_bytes: 0,
        estimated: false,
        cost: 1.0,
        latency_ms: 5,
        status: 200,
        metadata: None,
        ts,
    }
}

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs(),
    )
    .expect("fits i64")
}

/// Find the group named `name` in a report body.
fn group<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .find(|g| g["group"] == name)
        .unwrap_or_else(|| panic!("no group '{name}' in {body}"))
}

// ---- Auth gating --------------------------------------------------------------

#[tokio::test]
async fn admin_usage_requires_the_master_key() {
    let h = spawn_admin(common::empty_registry()).await;

    // No Authorization header: 401 with the LM-4004 envelope.
    let no_auth = h
        .client
        .get(format!("{}/admin/usage", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(no_auth.status(), 401);
    let body: Value = no_auth.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-4004");

    // A wrong master key.
    let wrong = h
        .client
        .get(format!("{}/admin/usage", h.base))
        .bearer_auth("b".repeat(64))
        .send()
        .await
        .expect("send");
    assert_eq!(wrong.status(), 401);

    // A virtual key is NOT an admin key.
    let vkey = h.create_key().await;
    let with_vkey = h
        .client
        .get(format!("{}/admin/usage", h.base))
        .bearer_auth(&vkey)
        .send()
        .await
        .expect("send");
    assert_eq!(with_vkey.status(), 401);
}

#[tokio::test]
async fn admin_usage_does_not_exist_when_auth_is_disabled() {
    let base = common::spawn().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/usage"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}

// ---- Defaults & aggregation ----------------------------------------------------

#[tokio::test]
async fn defaults_to_last_24h_grouped_by_model() {
    let h = spawn_admin(common::empty_registry()).await;
    let now = now_unix();
    // Two recent rows on "gpt" (one estimated, one 402 rejection), one recent
    // row on another model, and one row 2 days old that must NOT be counted.
    let mut estimated = row(now - 10);
    estimated.estimated = true;
    estimated.tokens_in = 5;
    estimated.tokens_out = 0;
    estimated.cost = 0.5;
    let mut rejected = row(now - 20);
    rejected.status = 402;
    rejected.tokens_in = 0;
    rejected.tokens_out = 0;
    rejected.cost = 0.0;
    let mut other_model = row(now - 30);
    other_model.model = "claude".to_owned();
    other_model.model_used = "claude".to_owned();
    other_model.provider = "anthropic".to_owned();
    let stale = row(now - 2 * 86_400);
    h.store
        .insert_usage(&[row(now - 5), estimated, rejected, other_model, stale])
        .await
        .expect("seed");

    let resp = h.usage("").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["group_by"], "model");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["groups"].as_array().expect("groups").len(), 2);

    let gpt = group(&body, "gpt");
    assert_eq!(gpt["requests"], 3);
    assert_eq!(gpt["requests_ok"], 2);
    assert_eq!(gpt["requests_client_error"], 1);
    assert_eq!(gpt["requests_server_error"], 0);
    assert_eq!(gpt["tokens_in"], 15); // 10 + 5 + 0
    assert_eq!(gpt["tokens_out"], 20);
    assert_eq!(gpt["tokens_total"], 35);
    assert_eq!(gpt["estimated_requests"], 1);
    assert_eq!(gpt["upstream_requests"], 2);
    assert_eq!(gpt["cost"], 1.5);

    let claude = group(&body, "claude");
    assert_eq!(claude["requests"], 1);
    assert_eq!(claude["tokens_total"], 30);
}

#[tokio::test]
async fn each_filter_narrows_the_result() {
    let h = spawn_admin(common::empty_registry()).await;
    let now = now_unix();
    let mut other_key = row(now - 1);
    other_key.key_id = Some("key-b".to_owned());
    let mut embed = row(now - 2);
    embed.capability = "embed".to_owned();
    embed.model = "embed-small".to_owned();
    embed.model_used = "embed-small".to_owned();
    let mut rerank = row(now - 3);
    rerank.capability = "rerank".to_owned();
    rerank.model = "rerank-fast".to_owned();
    rerank.model_used = "rerank-fast".to_owned();
    rerank.provider = "cohere".to_owned();
    rerank.search_units = Some(2);
    h.store
        .insert_usage(&[row(now - 4), other_key, embed, rerank])
        .await
        .expect("seed");

    // key_id
    let body: Value = h
        .usage("?key_id=key-b&group_by=key_id")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    assert_eq!(body["groups"][0]["group"], "key-b");
    assert_eq!(body["groups"][0]["requests"], 1);

    // model
    let body: Value = h.usage("?model=gpt").await.json().await.expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    let gpt = group(&body, "gpt");
    assert_eq!(gpt["requests"], 2); // row + other_key

    // provider
    let body: Value = h
        .usage("?provider=cohere&group_by=provider")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    let cohere = group(&body, "cohere");
    assert_eq!(cohere["requests"], 1);
    assert_eq!(cohere["search_units"], 2);

    // capability
    let body: Value = h
        .usage("?capability=embed&group_by=capability")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    let embed_group = group(&body, "embed");
    assert_eq!(embed_group["requests"], 1);
}

#[tokio::test]
async fn time_window_accepts_unix_and_rfc3339() {
    let h = spawn_admin(common::empty_registry()).await;
    // Fixed, timezone-independent timestamps: 2026-07-15T00:00:00Z is
    // 1784073600 unix.
    let base_ts = 1_784_073_600_i64;
    let mut early = row(base_ts - 3_600);
    early.model = "early".to_owned();
    let mut inside = row(base_ts + 60);
    inside.model = "inside".to_owned();
    let mut late = row(base_ts + 7_200);
    late.model = "late".to_owned();
    h.store
        .insert_usage(&[early, inside, late])
        .await
        .expect("seed");

    // Unix-seconds window around `inside` only.
    let query = format!("?since={}&until={}", base_ts, base_ts + 3_600);
    let body: Value = h.usage(&query).await.json().await.expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    assert_eq!(body["groups"][0]["group"], "inside");
    assert_eq!(body["since"], base_ts);
    assert_eq!(body["until"], base_ts + 3_600);

    // The same window, spelled in RFC3339 (with an offset variant).
    let body: Value = h
        .usage("?since=2026-07-15T00:00:00Z&until=2026-07-15T03:00:00%2B02:00")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    assert_eq!(body["groups"][0]["group"], "inside");
    assert_eq!(body["since"], base_ts);
    assert_eq!(body["until"], base_ts + 3_600);
}

#[tokio::test]
async fn group_by_total_returns_one_row_and_status_splits_by_status() {
    let h = spawn_admin(common::empty_registry()).await;
    let now = now_unix();
    let mut failed = row(now - 1);
    failed.status = 502;
    failed.tokens_in = 0;
    failed.tokens_out = 0;
    failed.cost = 0.0;
    h.store
        .insert_usage(&[row(now - 2), failed])
        .await
        .expect("seed");

    let body: Value = h.usage("?group_by=total").await.json().await.expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 1);
    let total = group(&body, "total");
    assert_eq!(total["requests"], 2);
    assert_eq!(total["requests_ok"], 1);
    assert_eq!(total["requests_server_error"], 1);

    let body: Value = h
        .usage("?group_by=status")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["groups"].as_array().expect("groups").len(), 2);
    assert_eq!(group(&body, "200")["requests"], 1);
    assert_eq!(group(&body, "502")["requests"], 1);
}

#[tokio::test]
async fn empty_window_returns_the_empty_shape() {
    let h = spawn_admin(common::empty_registry()).await;
    let now = now_unix();
    h.store.insert_usage(&[row(now)]).await.expect("seed");

    let body: Value = h
        .usage("?since=1000&until=2000")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["since"], 1000);
    assert_eq!(body["until"], 2000);
    assert_eq!(body["group_by"], "model");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["groups"], json!([]));
}

#[tokio::test]
async fn limit_bounds_the_groups_and_flags_truncation() {
    let h = spawn_admin(common::empty_registry()).await;
    let now = now_unix();
    let mut rows = Vec::new();
    // Distinct costs so the most expensive groups win under truncation.
    for (name, cost) in [("cheap", 1.0), ("mid", 2.0), ("pricey", 3.0)] {
        let mut r = row(now - 1);
        r.model = name.to_owned();
        r.cost = cost;
        rows.push(r);
    }
    h.store.insert_usage(&rows).await.expect("seed");

    let body: Value = h.usage("?limit=2").await.json().await.expect("json");
    let groups = body["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 2);
    assert_eq!(body["truncated"], true);
    // Ordered by cost, descending.
    assert_eq!(groups[0]["group"], "pricey");
    assert_eq!(groups[1]["group"], "mid");
}

// ---- Invalid parameters ---------------------------------------------------------

#[tokio::test]
async fn invalid_parameters_are_400_lm1001() {
    let h = spawn_admin(common::empty_registry()).await;

    for query in [
        "?group_by=bogus",
        "?since=not-a-time",
        "?until=2026-13-40T99:99:99Z",
        "?since=2000&until=1000",
        "?limit=0",
        "?limit=100000",
        "?capability=bogus",
        "?definitely_not_a_param=1",
    ] {
        let resp = h.usage(query).await;
        assert_eq!(resp.status(), 400, "query {query} must be rejected");
        let body: Value = resp.json().await.expect("json");
        assert_eq!(body["error"]["code"], "LM-1001", "query {query}");
    }
}

// ---- End to end through the gateway ----------------------------------------------

#[tokio::test]
async fn gateway_requests_show_up_with_their_provider() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 7, "total_tokens": 7 }
        })))
        .mount(&upstream)
        .await;
    let registry = Arc::new(
        Registry::build(
            vec![ProviderSpec {
                name: "openai".to_owned(),
                kind: ProviderKind::Openai,
                api_key: Some("sk-test-xxx".to_owned()),
                base_url: Some(upstream.uri()),
                strict: false,
                connect_timeout_ms: None,
                api_version: None,
                models: vec![ModelSpec {
                    id: "embed-small".to_owned(),
                    upstream_id: "text-embedding-3-small".to_owned(),
                    capabilities: vec![Capability::Embed],
                    modalities: vec!["text".to_owned()],
                }],
            }],
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let h = spawn_admin(registry).await;
    let key = h.create_key().await;

    let resp = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "embed-small", "input": "abcd" }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    h.wait_usage_rows(1).await;

    let body: Value = h
        .usage("?group_by=provider")
        .await
        .json()
        .await
        .expect("json");
    let openai = group(&body, "openai");
    assert_eq!(openai["requests"], 1);
    assert_eq!(openai["tokens_in"], 7);
    // Upstream reported the usage: nothing was estimated.
    assert_eq!(openai["estimated_requests"], 0);
    assert_eq!(openai["upstream_requests"], 1);
}
