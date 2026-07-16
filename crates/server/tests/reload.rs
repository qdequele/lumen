//! End-to-end test for M7 hot reload: rotating a DB-stored provider key
//! (`PUT /admin/provider-keys`) is picked up by a reload and the very next
//! upstream request authenticates with the new key - no restart. The upstream
//! is wiremock; LUMEN sits in front and we inspect the `Authorization` header
//! the provider actually sent.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use lumen_auth::crypto::MasterKey;
use lumen_auth::store::KeyStore;
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::reload::{reload_once, ProviderKeySource, ReloadTargets};
use lumen_server::resilience::ResilienceRuntime;
use lumen_telemetry::{Metrics, ReloadMetrics};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// The master-key value (64 hex chars) sealing provider keys in these tests.
fn master() -> MasterKey {
    MasterKey::from_env_value(&"a".repeat(64)).expect("master key")
}

/// A unique temp dir under the OS temp root (no external crate).
fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lumen-reload-it-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write a cohere-only config pointing at `upstream`, and return its path.
fn write_config(dir: &Path, upstream: &str) -> PathBuf {
    let body = format!(
        r#"
        [[providers]]
        name = "cohere"
        kind = "cohere"
        base_url = "{upstream}"
        [[providers.models]]
        id = "rr"
        upstream_id = "rerank-v3.5"
        capabilities = ["rerank"]
        "#
    );
    let path = dir.join("config.toml");
    let mut file = std::fs::File::create(&path).expect("create config");
    file.write_all(body.as_bytes()).expect("write config");
    path
}

/// Build the registry from `path`'s specs, merging a single provider key in
/// (mirroring the boot DB back-fill for an env-keyless provider).
fn registry_with_key(path: &Path, key: &str) -> Arc<Registry> {
    let config = Config::load(path).expect("config loads");
    let mut specs = config.provider_specs();
    for spec in &mut specs {
        if spec.name == "cohere" {
            spec.api_key = Some(key.to_owned());
        }
    }
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

async fn mount_rerank(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 0, "relevance_score": 0.9 }],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(upstream)
        .await;
}

/// The `Authorization` header of the most recent upstream request.
async fn last_upstream_auth(upstream: &MockServer) -> String {
    let reqs = upstream
        .received_requests()
        .await
        .expect("received requests");
    let last = reqs.last().expect("at least one upstream request");
    last.headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .expect("header is ascii")
        .to_owned()
}

async fn send_rerank(base: &str, client: &reqwest::Client) {
    let resp = client
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rr", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .expect("send rerank");
    assert_eq!(resp.status(), 200, "rerank succeeds");
}

#[tokio::test]
async fn rotating_a_db_provider_key_takes_effect_on_reload_without_restart() {
    let upstream = MockServer::start().await;
    mount_rerank(&upstream).await;
    let dir = tempdir();
    let path = write_config(&dir, &upstream.uri());

    // Seed the encrypted store with the ORIGINAL provider key.
    let store = KeyStore::in_memory().await.expect("store");
    store
        .store_provider_key("cohere", "old-key", &master())
        .await
        .expect("store old key");

    // Boot the gateway with the original key back-filled into the registry.
    let registry = registry_with_key(&path, "old-key");
    let state = common::base_state(Arc::clone(&registry));
    let base = common::spawn_state(state, LIMIT).await;
    let client = reqwest::Client::new();

    // First request goes out under the original key.
    send_rerank(&base, &client).await;
    assert_eq!(
        last_upstream_auth(&upstream).await,
        "Bearer old-key",
        "the boot key authenticates the first request"
    );

    // Rotate the key in the DB (as `PUT /admin/provider-keys` would), then run
    // exactly one reload through the real reloader entry point.
    store
        .store_provider_key("cohere", "new-key", &master())
        .await
        .expect("rotate key");
    let targets = Arc::new(ReloadTargets {
        registry: Arc::clone(&registry),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics: ReloadMetrics::register(&Metrics::new()).expect("reload metrics"),
        key_backfill: Arc::new(ArcSwap::from_pointee(single("cohere", "old-key"))),
        key_source: Some(Arc::new(ProviderKeySource::new(
            store.clone(),
            master(),
            vec!["cohere".to_owned()],
        ))),
        auth_knobs: None,
        auth_runtime: None,
    });
    reload_once(&path, &targets).await;

    // The next request - through the SAME running gateway - uses the new key.
    send_rerank(&base, &client).await;
    assert_eq!(
        last_upstream_auth(&upstream).await,
        "Bearer new-key",
        "the rotated DB key authenticates requests after a reload, no restart"
    );
}

fn single(k: &str, v: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(k.to_owned(), v.to_owned());
    m
}
