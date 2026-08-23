//! Integration tests for `GET /admin/usage/export` (ADR 010): master-key
//! gating, cursor pagination, the null-cursor-on-last-page contract and
//! parameter validation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewKey, UsageRecord};
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_providers::Registry;
use lumen_server::auth::AuthRuntime;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::Value;

const LIMIT: usize = 10 * 1024 * 1024;

/// The master key value (64 hex chars) used as the admin bearer token.
fn master() -> String {
    "a".repeat(64)
}

struct Harness {
    base: String,
    store: KeyStore,
    // Mirrors `admin_usage.rs`'s `Harness` for consistency, but this suite
    // never needs direct `AuthRuntime` access (no live key/group upserts).
    #[allow(dead_code)]
    runtime: Arc<AuthRuntime>,
    client: reqwest::Client,
}

impl Harness {
    /// GET an admin path with the master key.
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("request sent")
    }

    /// Insert `n` usage rows one second apart, all attributed to one key.
    ///
    /// Timestamped relative to "now" (not a fixed epoch) so the rows fall
    /// inside the endpoint's default 24-hour window: the handler defaults
    /// `since`/`until` the same way `usage_report` does, so a fixed
    /// `ts: 1_000 + i` (1970) would silently fall outside every query in
    /// these tests that omits `since`/`until`.
    async fn seed_usage(&self, n: i64) {
        let (_plaintext, record) = self
            .store
            .create_key(NewKey {
                name: "seeded".to_owned(),
                group_id: None,
                budget_max: None,
                rpm_limit: None,
                tpm_limit: None,
                expires_at: None,
            })
            .await
            .expect("create key");
        let id = record.id;
        let base_ts = now_unix() - n;
        let batch: Vec<UsageRecord> = (0..n)
            .map(|i| UsageRecord {
                key_id: Some(id.clone()),
                group_id: None,
                model: "gpt-test".to_owned(),
                model_used: "gpt-test".to_owned(),
                provider: "openai".to_owned(),
                capability: "chat".to_owned(),
                tokens_in: 10,
                tokens_out: 20,
                search_units: None,
                cached_tokens: None,
                reasoning_tokens: None,
                cache_write_tokens: None,
                media_count: 0,
                media_bytes: 0,
                estimated: false,
                cost: 0.001,
                latency_ms: 5,
                status: 200,
                metadata: None,
                ts: base_ts + i,
            })
            .collect();
        self.store.insert_usage(&batch).await.expect("seed usage");
    }
}

/// Current unix time, for seeding usage rows inside the default window.
fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs(),
    )
    .expect("fits i64")
}

/// The same minimal registry `admin_usage.rs` uses: no providers configured,
/// since these tests only exercise the admin usage-export route.
fn registry() -> Arc<Registry> {
    common::empty_registry()
}

/// Spawn an auth-enabled gateway around `registry`.
async fn spawn_admin(registry: Arc<Registry>) -> Harness {
    let store = KeyStore::in_memory().await.expect("open store");
    let groups = store.load_groups().await.expect("load groups");
    let entries = store.load_auth_entries().await.expect("load entries");
    let keys = AuthState::load(groups, entries);
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

#[tokio::test]
async fn export_requires_the_master_key() {
    let h = spawn_admin(registry()).await;
    let response = h
        .client
        .get(format!("{}/admin/usage/export", h.base))
        .send()
        .await
        .expect("request sent");
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn export_pages_and_reports_the_next_cursor() {
    let h = spawn_admin(registry()).await;
    h.seed_usage(5).await;

    let body: Value = h
        .get("/admin/usage/export?limit=2")
        .await
        .json()
        .await
        .expect("json body");
    let rows = body["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2);
    let cursor = body["next_cursor"].as_i64().expect("a cursor");

    let body2: Value = h
        .get(&format!("/admin/usage/export?limit=2&cursor={cursor}"))
        .await
        .json()
        .await
        .expect("json body");
    let rows2 = body2["rows"].as_array().expect("rows array");
    assert_eq!(rows2.len(), 2);
    assert!(rows2[0]["id"].as_i64().expect("id") > cursor);
}

#[tokio::test]
async fn export_reports_a_null_cursor_on_the_last_page() {
    let h = spawn_admin(registry()).await;
    h.seed_usage(2).await;

    let body: Value = h
        .get("/admin/usage/export?limit=100")
        .await
        .json()
        .await
        .expect("json body");
    assert_eq!(body["rows"].as_array().expect("rows").len(), 2);
    assert!(
        body["next_cursor"].is_null(),
        "a short page means the window is exhausted"
    );
}

#[tokio::test]
async fn export_rejects_an_unknown_query_parameter() {
    let h = spawn_admin(registry()).await;
    let response = h.get("/admin/usage/export?limitt=2").await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn export_caps_an_oversized_limit() {
    let h = spawn_admin(registry()).await;
    h.seed_usage(3).await;
    let response = h.get("/admin/usage/export?limit=999999").await;
    assert_eq!(
        response.status(),
        400,
        "a limit above the hard cap is the caller's error, not a silent clamp"
    );
}
