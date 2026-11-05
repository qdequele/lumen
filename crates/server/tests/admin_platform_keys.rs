//! Platform provider key pools (ADR 019): the platform-only admin API under
//! `/admin/platform-keys`. Keys are sealed at rest, never returned and never
//! logged; every write pings the reload trigger; a call scoped to an account
//! is refused.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_server::auth::AuthRuntime;
use lumen_server::config_source::ConfigContext;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Notify;

const MASTER: &str = "admin-master-token";
const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const HEADER: &str = "X-Lumen-Account-Ref";
const SECRET: &str = "sk-platform-SECRET-do-not-leak-0042";

const CONFIG_TOML: &str = r#"
[[providers]]
name = "openai-main"
kind = "openai"

[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]

[[providers]]
name = "azure-main"
kind = "azure"
base_url = "https://example.openai.azure.com"

[[providers.models]]
id = "az-gpt"
capabilities = ["chat"]
"#;

struct Harness {
    base: String,
    client: reqwest::Client,
    store: KeyStore,
    state: AppState,
    trigger: Arc<Notify>,
    _dir: TempDir,
}

async fn spawn() -> Harness {
    let dir = TempDir::new().expect("temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, CONFIG_TOML).expect("write config");

    let store = KeyStore::in_memory().await.expect("store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key(MASTER),
        master: Some(MasterKey::from_env_value(&"a".repeat(64)).expect("master key")),
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("tokens");
    let latency = LatencyMetrics::register(&metrics).expect("latency");
    let trigger = Arc::new(Notify::new());
    let state = AppState::new(metrics, common::empty_registry(), tokens, latency)
        .with_auth(runtime)
        .with_config_context(Arc::new(ConfigContext::file(config_path)))
        .with_reload_trigger(Arc::clone(&trigger));
    let base = common::spawn_state(state.clone(), 1 << 20).await;
    Harness {
        base,
        client: reqwest::Client::new(),
        store,
        state,
        trigger,
        _dir: dir,
    }
}

impl Harness {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        scope: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(MASTER);
        if let Some(scope) = scope {
            req = req.header(HEADER, scope);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("send");
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn add(&self, provider: &str, key: &str, label: &str) -> (u16, Value) {
        self.call(
            reqwest::Method::POST,
            &format!("/admin/platform-keys/{provider}/keys"),
            None,
            Some(json!({"key": key, "label": label})),
        )
        .await
    }

    async fn list(&self) -> Value {
        let (status, listing) = self
            .call(reqwest::Method::GET, "/admin/platform-keys", None, None)
            .await;
        assert_eq!(status, 200, "{listing}");
        listing
    }

    async fn pinged(&self) {
        tokio::time::timeout(Duration::from_secs(1), self.trigger.notified())
            .await
            .expect("reload trigger pinged");
    }
}

fn provider<'a>(listing: &'a Value, name: &str) -> &'a Value {
    listing["providers"]
        .as_array()
        .expect("providers array")
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("provider {name} missing in {listing}"))
}

#[tokio::test]
async fn add_list_toggle_delete_platform_key() {
    let h = spawn().await;

    let (status, created) = h.add("openai-main", SECRET, " primary ").await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["label"], "primary");
    assert_eq!(created["last4"], "0042");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["status"], "unused");
    assert!(created["id"].as_str().expect("id").starts_with("ppk_"));
    assert!(created["created_at"].is_i64());
    assert!(created.get("key").is_none());
    h.pinged().await;

    let listing = h.list().await;
    let text = listing.to_string();
    assert!(!text.contains(SECRET), "plaintext leaked: {text}");
    let openai = provider(&listing, "openai-main");
    assert_eq!(openai["kind"], "openai");
    assert_eq!(openai["eligible"], true);
    assert_eq!(openai["configured_key"], false);
    assert_eq!(openai["models"], json!(["gpt-4o"]));
    assert_eq!(openai["keys"][0]["label"], "primary");
    assert_eq!(provider(&listing, "azure-main")["eligible"], false);
    assert_eq!(provider(&listing, "azure-main")["keys"], json!([]));

    let id = created["id"].as_str().expect("id").to_owned();
    let path = format!("/admin/platform-keys/openai-main/keys/{id}");
    let (status, toggled) = h
        .call(
            reqwest::Method::PATCH,
            &path,
            None,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, 200, "{toggled}");
    assert_eq!(toggled["enabled"], false);
    assert_eq!(toggled["id"], id.as_str());
    h.pinged().await;
    assert_eq!(
        provider(&h.list().await, "openai-main")["keys"][0]["enabled"],
        false
    );

    let (status, _) = h.call(reqwest::Method::DELETE, &path, None, None).await;
    assert_eq!(status, 204);
    h.pinged().await;
    assert_eq!(provider(&h.list().await, "openai-main")["keys"], json!([]));

    let (status, error) = h.call(reqwest::Method::DELETE, &path, None, None).await;
    assert_eq!(status, 404, "a second delete is a 404");
    assert_eq!(error["error"]["code"], "LM-1003");
    let (status, error) = h
        .call(
            reqwest::Method::PATCH,
            &path,
            None,
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, 404, "{error}");
}

#[tokio::test]
async fn platform_key_is_sealed_at_rest() {
    let h = spawn().await;
    let (status, created) = h.add("openai-main", SECRET, "primary").await;
    assert_eq!(status, 201, "{created}");
    let dump = h.store.debug_dump().await.expect("dump");
    assert!(
        dump.contains(created["id"].as_str().expect("id")),
        "the dump must cover platform_provider_keys"
    );
    assert!(!dump.contains(SECRET), "the key must be encrypted at rest");
}

#[tokio::test]
async fn invalid_writes_are_rejected() {
    let h = spawn().await;
    let cases = [
        ("openai-main", "short-key", "x", 400),
        ("openai-main", SECRET, "", 400),
        ("openai-main", SECRET, &"l".repeat(65), 400),
        ("azure-main", SECRET, "x", 400),
        ("nope", SECRET, "x", 404),
    ];
    for (name, key, label, expected) in cases {
        let (status, error) = h.add(name, key, label).await;
        assert_eq!(status, expected, "{name} {label}: {error}");
        assert!(!error.to_string().contains(SECRET), "{error}");
    }
    let (status, _) = h
        .call(
            reqwest::Method::POST,
            "/admin/platform-keys/openai-main/keys",
            None,
            Some(json!({"key": SECRET, "label": "x", "extra": 1})),
        )
        .await;
    assert_eq!(status, 400, "unknown fields are refused");
    let stored = h.store.list_platform_provider_keys().await.expect("list");
    assert_eq!(stored.len(), 0, "nothing was stored");
}

#[tokio::test]
async fn platform_key_routes_refuse_an_account_scoped_call() {
    let h = spawn().await;
    let (_, created) = h.add("openai-main", SECRET, "primary").await;
    let id = created["id"].as_str().expect("id").to_owned();
    let attempts = [
        (
            reqwest::Method::GET,
            "/admin/platform-keys".to_owned(),
            None,
        ),
        (
            reqwest::Method::POST,
            "/admin/platform-keys/openai-main/keys".to_owned(),
            Some(json!({"key": SECRET, "label": "x"})),
        ),
        (
            reqwest::Method::PATCH,
            format!("/admin/platform-keys/openai-main/keys/{id}"),
            Some(json!({"enabled": false})),
        ),
        (
            reqwest::Method::DELETE,
            format!("/admin/platform-keys/openai-main/keys/{id}"),
            None,
        ),
    ];
    for (method, path, body) in attempts {
        let (status, error) = h.call(method.clone(), &path, Some(ACCOUNT_A), body).await;
        assert_eq!(status, 403, "{method} {path}: {error}");
        assert_eq!(error["error"]["code"], "LM-4005", "{method} {path}");
    }
    let keys = h.store.list_platform_provider_keys().await.expect("list");
    assert_eq!(keys.len(), 1);
    assert!(keys[0].enabled, "nothing changed");
}

#[tokio::test]
async fn disable_and_delete_evict_the_key_from_the_live_pool_before_answering() {
    let h = spawn().await;
    let (_, created) = h.add("openai-main", SECRET, "primary").await;
    let id = created["id"].as_str().expect("id").to_owned();
    // Simulate the reloader having loaded the key.
    let master = MasterKey::from_env_value(&"a".repeat(64)).expect("master key");
    h.state.team_pools.replace_platform(
        h.store
            .load_platform_provider_keys(&master)
            .await
            .expect("load"),
    );
    assert!(h.state.team_pools.has_platform_key("openai-main", &id));

    let path = format!("/admin/platform-keys/openai-main/keys/{id}");
    let (status, _) = h
        .call(
            reqwest::Method::PATCH,
            &path,
            None,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!h.state.team_pools.has_platform_key("openai-main", &id));
}
