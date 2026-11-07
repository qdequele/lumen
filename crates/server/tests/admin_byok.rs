//! Team provider key pools (BYOK): the account-scoped admin API under
//! `/admin/groups/{id}/byok`. Keys are sealed at rest, never returned and
//! never logged; every write pings the reload trigger.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
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
const MASTER_KEY: &str = "a";
const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const ACCOUNT_B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";
const HEADER: &str = "X-Lumen-Account-Ref";
const SECRET: &str = "sk-team-SECRET-do-not-leak-0042";

const CONFIG_TOML: &str = r#"
[[providers]]
name = "openai-main"
kind = "openai"
api_key_env = "TEST_BYOK_PROVIDER_KEY"

[[providers.models]]
id = "openai-main/gpt-4o"
capabilities = ["chat"]

[[providers]]
name = "azure-main"
kind = "azure"
base_url = "https://example.openai.azure.com"

[[providers.models]]
id = "azure-main/az-gpt"
capabilities = ["chat"]

[[providers]]
name = "bedrock-main"
kind = "bedrock"

[[providers.models]]
id = "bedrock-main/br-claude"
capabilities = ["chat"]
"#;

/// Two eligible providers only: valid under `Config::load`, so the provider
/// delete route (which re-validates the whole document) accepts it.
const DELETABLE_TOML: &str = r#"
[[providers]]
name = "openai-main"
kind = "openai"
api_key_env = "TEST_BYOK_PROVIDER_KEY"

[[providers.models]]
id = "openai-main/gpt-4o"
capabilities = ["chat"]

[[providers]]
name = "groq-extra"
kind = "groq"
api_key_env = "TEST_BYOK_PROVIDER_KEY"

[[providers.models]]
id = "groq-extra/llama"
capabilities = ["chat"]
"#;

struct Harness {
    base: String,
    client: reqwest::Client,
    store: KeyStore,
    trigger: Arc<Notify>,
    config_hash_path: std::path::PathBuf,
    _dir: TempDir,
}

async fn spawn(config: &str) -> Harness {
    // Written once per process, before any server starts (see admin_config.rs).
    static ENV_ONCE: std::sync::Once = std::sync::Once::new();
    ENV_ONCE.call_once(|| std::env::set_var("TEST_BYOK_PROVIDER_KEY", "platform-key"));
    let dir = TempDir::new().expect("temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, config).expect("write config");

    let store = KeyStore::in_memory().await.expect("store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key(MASTER),
        master: Some(MasterKey::from_env_value(&MASTER_KEY.repeat(64)).expect("master key")),
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("tokens");
    let latency = LatencyMetrics::register(&metrics).expect("latency");
    let trigger = Arc::new(Notify::new());
    let state = AppState::new(metrics, common::empty_registry(), tokens, latency)
        .with_auth(runtime)
        .with_config_context(Arc::new(ConfigContext::file(config_path.clone())))
        .with_reload_trigger(Arc::clone(&trigger));
    let base = common::spawn_state(state, 1 << 20).await;
    Harness {
        base,
        client: reqwest::Client::new(),
        store,
        trigger,
        config_hash_path: config_path,
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

    /// A group of `account`, returns its id.
    async fn group(&self, account: &str) -> String {
        let (status, group) = self
            .call(
                reqwest::Method::POST,
                "/admin/groups",
                None,
                Some(json!({"name": account, "budget_max": 5.0, "account_ref": account})),
            )
            .await;
        assert_eq!(status, 201, "{group}");
        group["id"].as_str().expect("id").to_owned()
    }

    async fn add_key(&self, group: &str, provider: &str, key: &str, label: &str) -> (u16, Value) {
        self.call(
            reqwest::Method::POST,
            &format!("/admin/groups/{group}/byok/{provider}/keys"),
            Some(account_of(group, self).await),
            Some(json!({"key": key, "label": label})),
        )
        .await
    }

    async fn list(&self, group: &str) -> (u16, Value) {
        self.call(
            reqwest::Method::GET,
            &format!("/admin/groups/{group}/byok"),
            None,
            None,
        )
        .await
    }
}

/// Group id to the account it was created for, so helper calls stay scoped
/// like the Lab proxy's (the id is opaque; ask the store).
async fn account_of<'a>(group: &str, h: &'a Harness) -> &'a str {
    let record = h
        .store
        .get_group(group)
        .await
        .expect("get group")
        .expect("group exists");
    match record.account_ref.as_deref() {
        Some(ACCOUNT_A) => ACCOUNT_A,
        _ => ACCOUNT_B,
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
async fn add_list_delete_team_key() {
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;

    let (status, created) = h.add_key(&group, "openai-main", SECRET, " prod ").await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["label"], "prod");
    assert_eq!(created["last4"], "0042");
    assert_eq!(created["status"], "unused");
    assert!(created["status_detail"].is_null());
    assert!(created["id"].as_str().expect("id").starts_with("gpk_"));
    assert!(created["created_at"].is_i64());
    assert!(created.get("key").is_none());

    let (status, listing) = h.list(&group).await;
    assert_eq!(status, 200, "{listing}");
    let text = listing.to_string();
    assert!(!text.contains(SECRET), "plaintext leaked: {text}");
    assert!(!text.contains("\"key\""), "a `key` field leaked: {text}");
    let openai = provider(&listing, "openai-main");
    assert_eq!(openai["kind"], "openai");
    assert_eq!(openai["eligible"], true);
    assert_eq!(openai["fallback"], false);
    assert_eq!(openai["models"], json!(["openai-main/gpt-4o"]));
    assert_eq!(openai["keys"][0]["label"], "prod");
    assert_eq!(openai["keys"][0]["last4"], "0042");
    // Every provider of the document is listed, eligible or not.
    assert_eq!(provider(&listing, "azure-main")["eligible"], false);
    assert_eq!(provider(&listing, "bedrock-main")["eligible"], false);

    // Adding a key pings the reloader (L5 consumes it).
    tokio::time::timeout(Duration::from_secs(1), h.trigger.notified())
        .await
        .expect("reload trigger pinged");

    let id = created["id"].as_str().expect("id");
    let (status, _) = h
        .call(
            reqwest::Method::DELETE,
            &format!("/admin/groups/{group}/byok/openai-main/keys/{id}"),
            Some(ACCOUNT_A),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (_, listing) = h.list(&group).await;
    assert_eq!(provider(&listing, "openai-main")["keys"], json!([]));

    let (status, _) = h
        .call(
            reqwest::Method::DELETE,
            &format!("/admin/groups/{group}/byok/openai-main/keys/{id}"),
            Some(ACCOUNT_A),
            None,
        )
        .await;
    assert_eq!(status, 404, "a second delete is a 404");
}

#[tokio::test]
async fn team_key_is_sealed_at_rest() {
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    let (status, created) = h.add_key(&group, "openai-main", SECRET, "prod").await;
    assert_eq!(status, 201, "{created}");
    let dump = h.store.debug_dump().await.expect("dump");
    // Guards against a vacuous pass: the pool table is part of the dump.
    assert!(
        dump.contains(created["id"].as_str().expect("id")),
        "the dump must cover group_provider_keys"
    );
    assert!(!dump.contains(SECRET), "the key must be encrypted at rest");
}

#[tokio::test]
async fn team_key_routes_are_account_scoped() {
    let h = spawn(CONFIG_TOML).await;
    let group_a = h.group(ACCOUNT_A).await;
    let (status, created) = h.add_key(&group_a, "openai-main", SECRET, "prod").await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();

    let base = format!("/admin/groups/{group_a}/byok");
    let attempts = [
        (reqwest::Method::GET, base.clone(), None),
        (
            reqwest::Method::POST,
            format!("{base}/openai-main/keys"),
            Some(json!({"key": "sk-other-1234", "label": "x"})),
        ),
        (
            reqwest::Method::DELETE,
            format!("{base}/openai-main/keys/{id}"),
            None,
        ),
        (
            reqwest::Method::PUT,
            format!("{base}/openai-main"),
            Some(json!({"fallback": true})),
        ),
    ];
    for (method, path, body) in attempts {
        let (status, error) = h.call(method.clone(), &path, Some(ACCOUNT_B), body).await;
        assert_eq!(status, 404, "{method} {path}: {error}");
        assert_eq!(error["error"]["code"], "LM-1003", "{method} {path}");
    }
    // Nothing of A's pool changed.
    let (_, listing) = h.list(&group_a).await;
    assert_eq!(
        provider(&listing, "openai-main")["keys"]
            .as_array()
            .expect("keys")
            .len(),
        1
    );
    assert_eq!(provider(&listing, "openai-main")["fallback"], false);
}

#[tokio::test]
async fn ineligible_provider_kind_is_rejected() {
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    for name in ["azure-main", "bedrock-main"] {
        let (status, error) = h.add_key(&group, name, SECRET, "prod").await;
        assert_eq!(status, 400, "{name}: {error}");
        assert_eq!(error["error"]["code"], "LM-1001");
    }
    let (status, error) = h.add_key(&group, "nope", SECRET, "prod").await;
    assert_eq!(status, 404, "{error}");
    // Empty key and bad labels are 400 too.
    for (key, label) in [
        ("  ", "ok"),
        (SECRET, ""),
        (SECRET, "   "),
        (SECRET, &"x".repeat(65)),
    ] {
        let (status, error) = h.add_key(&group, "openai-main", key, label).await;
        assert_eq!(status, 400, "key={key:?} label={label:?}: {error}");
        assert_eq!(error["error"]["code"], "LM-1001");
    }
    let (_, listing) = h.list(&group).await;
    assert_eq!(provider(&listing, "openai-main")["keys"], json!([]));
}

#[tokio::test]
async fn short_key_is_rejected() {
    // `last4` of a short key would reveal most of it: under 16 characters
    // (after trimming) is refused.
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    for key in ["sk-short-123456", "   sk-short-123456   ", "abcd"] {
        let (status, error) = h.add_key(&group, "openai-main", key, "prod").await;
        assert_eq!(status, 400, "key={key:?}: {error}");
        assert_eq!(error["error"]["code"], "LM-1001");
        assert_eq!(
            error["error"]["message"], "invalid request: `key` must be at least 16 characters",
            "{error}"
        );
    }
    let (_, listing) = h.list(&group).await;
    assert_eq!(provider(&listing, "openai-main")["keys"], json!([]));
    // Exactly 16 characters is accepted.
    let (status, created) = h
        .add_key(&group, "openai-main", " sk-short-1234567 ", "prod")
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["last4"], "4567");
}

#[tokio::test]
async fn eleventh_key_is_rejected() {
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    for n in 0..10 {
        let (status, body) = h
            .add_key(&group, "openai-main", &format!("sk-team-key-{n:04}"), "k")
            .await;
        assert_eq!(status, 201, "key {n}: {body}");
    }
    let (status, error) = h
        .add_key(&group, "openai-main", "sk-team-key-0011", "k")
        .await;
    assert_eq!(status, 400, "{error}");
    assert_eq!(error["error"]["code"], "LM-1001");
    let (_, listing) = h.list(&group).await;
    assert_eq!(
        provider(&listing, "openai-main")["keys"]
            .as_array()
            .expect("keys")
            .len(),
        10
    );
}

#[tokio::test]
async fn fallback_toggle_round_trips() {
    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    let path = format!("/admin/groups/{group}/byok/openai-main");
    let (status, _) = h
        .call(
            reqwest::Method::PUT,
            &path,
            Some(ACCOUNT_A),
            Some(json!({"fallback": true})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, listing) = h.list(&group).await;
    assert_eq!(provider(&listing, "openai-main")["fallback"], true);
    let (status, _) = h
        .call(
            reqwest::Method::PUT,
            &path,
            Some(ACCOUNT_A),
            Some(json!({"fallback": false})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, listing) = h.list(&group).await;
    assert_eq!(provider(&listing, "openai-main")["fallback"], false);
    // An unknown provider is a 404.
    let (status, _) = h
        .call(
            reqwest::Method::PUT,
            &format!("/admin/groups/{group}/byok/nope"),
            Some(ACCOUNT_A),
            Some(json!({"fallback": true})),
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn deleting_provider_deletes_team_keys() {
    let h = spawn(DELETABLE_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    let (status, _) = h.add_key(&group, "groq-extra", SECRET, "prod").await;
    assert_eq!(status, 201);
    let (status, _) = h
        .call(
            reqwest::Method::PUT,
            &format!("/admin/groups/{group}/byok/groq-extra"),
            Some(ACCOUNT_A),
            Some(json!({"fallback": true})),
        )
        .await;
    assert_eq!(status, 204);

    let doc = std::fs::read_to_string(&h.config_hash_path).expect("config");
    let hash = {
        let (_, body) = h
            .call(reqwest::Method::GET, "/admin/config", None, None)
            .await;
        assert_eq!(body["config"], doc);
        body["hash"].as_str().expect("hash").to_owned()
    };
    let resp = h
        .client
        .delete(format!("{}/admin/config/providers/groq-extra", h.base))
        .bearer_auth(MASTER)
        .header("If-Match", hash)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 204);

    let master = MasterKey::from_env_value(&MASTER_KEY.repeat(64)).expect("master key");
    let keys = h
        .store
        .load_group_provider_keys(&master)
        .await
        .expect("load");
    assert_eq!(keys.len(), 0);
    let settings = h.store.group_provider_settings().await.expect("settings");
    assert_eq!(settings.len(), 0);
}

#[derive(Clone)]
struct BufMakeWriter(Arc<Mutex<Vec<u8>>>);
struct BufGuard(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMakeWriter {
    type Writer = BufGuard;
    fn make_writer(&'a self) -> Self::Writer {
        BufGuard(self.0.clone())
    }
}

impl Write for BufGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn team_key_secret_never_logged_on_admin_writes() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufMakeWriter(buffer.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // Another test may have cached "never" for these callsites first.
    tracing::callsite::rebuild_interest_cache();

    let h = spawn(CONFIG_TOML).await;
    let group = h.group(ACCOUNT_A).await;
    let (status, created) = h.add_key(&group, "openai-main", SECRET, "prod").await;
    assert_eq!(status, 201, "{created}");
    // Failure paths log too: ineligible kind, bad label, wrong scope.
    let _ = h.add_key(&group, "azure-main", SECRET, "prod").await;
    let _ = h.add_key(&group, "openai-main", SECRET, "").await;
    let _ = h
        .call(
            reqwest::Method::POST,
            &format!("/admin/groups/{group}/byok/openai-main/keys"),
            Some(ACCOUNT_B),
            Some(json!({"key": SECRET, "label": "x"})),
        )
        .await;
    let id = created["id"].as_str().expect("id");
    let _ = h
        .call(
            reqwest::Method::DELETE,
            &format!("/admin/groups/{group}/byok/openai-main/keys/{id}"),
            Some(ACCOUNT_A),
            None,
        )
        .await;

    let logs = String::from_utf8_lossy(&buffer.lock().expect("log buffer")).into_owned();
    assert!(!logs.is_empty(), "the subscriber captured nothing");
    assert!(!logs.contains(SECRET), "the team key reached the logs");
    assert!(
        !logs.contains("-SECRET-"),
        "part of the team key reached the logs"
    );
}

#[tokio::test]
async fn a_sub_group_has_no_team_key_routes() {
    // ADR 020: team keys live on the top-level group and serve every
    // sub-group; a sub-group's own team key routes are the unknown-id 404.
    let h = spawn(CONFIG_TOML).await;
    let team = h.group(ACCOUNT_A).await;
    let (status, sub) = h
        .call(
            reqwest::Method::POST,
            "/admin/groups",
            Some(ACCOUNT_A),
            Some(json!({"name": "sub", "parent_id": team})),
        )
        .await;
    assert_eq!(status, 201, "{sub}");
    let sub = sub["id"].as_str().expect("id");

    for scope in [None, Some(ACCOUNT_A)] {
        let (status, body) = h
            .call(
                reqwest::Method::GET,
                &format!("/admin/groups/{sub}/byok"),
                scope,
                None,
            )
            .await;
        assert_eq!(status, 404, "{body}");
        let (status, body) = h
            .call(
                reqwest::Method::POST,
                &format!("/admin/groups/{sub}/byok/openai-main/keys"),
                scope,
                Some(json!({"key": SECRET, "label": "x"})),
            )
            .await;
        assert_eq!(status, 404, "{body}");
    }
    let (status, body) = h.list(&team).await;
    assert_eq!(status, 200, "{body}");
}
