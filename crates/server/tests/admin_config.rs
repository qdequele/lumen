//! Integration tests for `GET /admin/config` (ADR 010): master-key gating,
//! and the central design constraint that the response is the config file's
//! raw bytes, never a re-serialisation of the in-memory `Config` (which would
//! render environment overrides as if they were file content).

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_providers::Registry;
use lumen_server::auth::AuthRuntime;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::Value;
use tempfile::TempDir;

const LIMIT: usize = 10 * 1024 * 1024;

/// The master key value (64 hex chars) used as the admin bearer token.
fn master() -> String {
    "a".repeat(64)
}

/// Name of the env var the harness config references via `api_key_env`.
/// Distinctive so it cannot race any other test in this workspace.
const PROVIDER_KEY_ENV: &str = "LUMEN_TEST_PROVIDER_KEY";
/// The sentinel value exported under `PROVIDER_KEY_ENV`. Must never appear in
/// the rendered config: only the env var *name* is file content.
const PROVIDER_KEY_VALUE: &str = "sentinel-provider-secret";

/// Config file content the harness boots with: a provider that resolves its
/// key from `PROVIDER_KEY_ENV`, so the "never expose a resolved value" test
/// has something to resolve.
const CONFIG_TOML: &str = r#"
[[providers]]
name = "test-provider"
kind = "openai"
api_key_env = "LUMEN_TEST_PROVIDER_KEY"

[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
"#;

struct Harness {
    base: String,
    client: reqwest::Client,
    /// Path of the on-disk config file `AppState::config_path` points at.
    config_path: std::path::PathBuf,
    /// Kept alive for the harness's lifetime: dropping it would delete
    /// `config_path` out from under the running server.
    _dir: TempDir,
}

impl Harness {
    /// GET an admin path with the master key.
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("send")
    }
}

/// The same minimal registry the other admin test suites use: no providers
/// configured, since these tests only exercise the admin config route.
fn registry() -> Arc<Registry> {
    common::empty_registry()
}

/// Spawn an auth-enabled gateway booted against a real config file on disk.
async fn spawn_admin(registry: Arc<Registry>) -> Harness {
    // Distinctive var name (see `PROVIDER_KEY_ENV` doc comment) so setting it
    // process-wide cannot race any other test in the workspace.
    std::env::set_var(PROVIDER_KEY_ENV, PROVIDER_KEY_VALUE);

    let dir = TempDir::new().expect("create temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, CONFIG_TOML).expect("write config file");

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
        .with_usage(logger)
        .with_config_path(config_path.clone());
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        client: reqwest::Client::new(),
        config_path,
        _dir: dir,
    }
}

#[tokio::test]
async fn get_config_requires_the_master_key() {
    let h = spawn_admin(registry()).await;
    let response = h
        .client
        .get(format!("{}/admin/config", h.base))
        .send()
        .await
        .expect("request sent");
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn get_config_returns_the_file_bytes_verbatim() {
    let h = spawn_admin(registry()).await;
    let on_disk = std::fs::read_to_string(&h.config_path).expect("read config");

    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(
        body["config"].as_str().expect("config string"),
        on_disk,
        "the operator sees the file, not a re-serialised merge of file and env"
    );
    assert_eq!(body["hash"].as_str().expect("hash").len(), 64);
}

#[tokio::test]
async fn get_config_never_exposes_a_resolved_provider_key() {
    // The harness config sets api_key_env = "LUMEN_TEST_PROVIDER_KEY" and the
    // harness exports that variable with a sentinel value.
    let h = spawn_admin(registry()).await;
    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    let rendered = body["config"].as_str().expect("config string");
    assert!(
        !rendered.contains(PROVIDER_KEY_VALUE),
        "the config exposes the env var NAME, never its resolved value"
    );
    assert!(rendered.contains(PROVIDER_KEY_ENV));
}
