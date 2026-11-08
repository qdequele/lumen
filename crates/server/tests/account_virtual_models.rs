//! ADR 022 acceptance: account virtual models over the real HTTP stack.
//! Isolation between two accounts, id collisions, platform deletes with
//! dependent account models, `If-Match` per account, `/v1/models` scoping
//! and the cross-process refresh.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_providers::{http, Registry};
use lumen_server::auth::AuthRuntime;
use lumen_server::config::Config;
use lumen_server::config_source::ConfigContext;
use lumen_server::pricing::CostTable;
use lumen_server::reload::{spawn_config_reloader, ReloadTargets};
use lumen_server::resilience::ResilienceRuntime;
use lumen_telemetry::{Metrics, ReloadMetrics};
use reqwest::Method;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Notify;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "account-models-master";
const A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";
const HEADER: &str = "X-Lumen-Account-Ref";
const PLATFORM_SECRET: &str = "PLATFORM-PRESET-TEXT";

fn config_toml(upstream: &str) -> String {
    format!(
        r#"
[resilience]
retry_max_attempts = 1

[[providers]]
name = "p"
kind = "openai"
base_url = "{upstream}"

[[providers.models]]
id = "p/chat-a"
capabilities = ["chat"]

[[providers.models]]
id = "p/chat-b"
capabilities = ["chat"]

[[virtual_models]]
id = "lumen/chat"
capability = "chat"
strategy = "single"
targets = [ {{ model = "p/chat-a" }} ]
preset = {{ system_prompt = "{PLATFORM_SECRET}" }}
"#
    )
}

struct H {
    base: String,
    client: reqwest::Client,
    resilience: Arc<ResilienceRuntime>,
    store: KeyStore,
    _upstream: MockServer,
    _dir: TempDir,
}

/// An upstream answering for both foundation models, echoing which one was
/// asked for in the completion's `model`.
async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    for model in ["chat-a", "chat-b"] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(json!({ "model": model })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "chat.completion", "id": "c", "created": 1, "model": model,
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" },
                              "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
            })))
            .mount(&server)
            .await;
    }
    server
}

/// An auth-enabled gateway over a config file with the hot reloader armed,
/// sharing its resilience runtime with the reloader (as `main.rs` does).
async fn spawn() -> H {
    let upstream = upstream().await;
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, config_toml(&upstream.uri())).unwrap();
    let config = Config::load(&config_path).unwrap();
    let registry = Arc::new(
        Registry::build(
            config.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .unwrap(),
    );
    let resilience = Arc::new(ResilienceRuntime::from_config(&config, None));
    let store = KeyStore::in_memory().await.unwrap();
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key(MASTER),
        master: None,
    });
    let ctx = Arc::new(ConfigContext::file(config_path));
    let trigger = Arc::new(Notify::new());
    let targets = ReloadTargets {
        registry: Arc::clone(&registry),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::clone(&resilience),
        metrics: ReloadMetrics::register(&Metrics::new()).unwrap(),
        key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        key_source: None,
        auth_knobs: None,
        webhooks: None,
        auth_runtime: None,
        image_fetch: None,
        token_counter: None,
        health: None,
        team_pools: None,
    };
    spawn_config_reloader(Arc::clone(&ctx), targets, Arc::clone(&trigger)).unwrap();
    let state = common::base_state(registry)
        .with_auth(runtime)
        .with_resilience(Arc::clone(&resilience))
        .with_pricing(CostTable::from_config(&config))
        .with_config_context(ctx)
        .with_reload_trigger(trigger);
    H {
        base: common::spawn_state(state, 1 << 20).await,
        client: reqwest::Client::new(),
        resilience,
        store,
        _upstream: upstream,
        _dir: dir,
    }
}

impl H {
    /// One admin call; `scope` sets the account header, `if_match` the
    /// version.
    async fn admin(
        &self,
        method: Method,
        path: &str,
        scope: Option<&str>,
        if_match: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(MASTER);
        if let Some(scope) = scope {
            req = req.header(HEADER, scope);
        }
        if let Some(if_match) = if_match {
            req = req.header("If-Match", if_match);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// The account's (or, unscoped, the platform's) virtual-model list.
    async fn list(&self, scope: Option<&str>) -> Value {
        let (status, body) = self
            .admin(
                Method::GET,
                "/admin/config/virtual_models",
                scope,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn hash(&self, scope: Option<&str>) -> String {
        self.list(scope).await["hash"].as_str().unwrap().to_owned()
    }

    /// PUT a virtual model with the current hash; returns the status/body.
    async fn put(&self, scope: Option<&str>, vm: &Value) -> (u16, Value) {
        let hash = self.hash(scope).await;
        let id = vm["id"].as_str().unwrap().replace('/', "%2F");
        self.admin(
            Method::PUT,
            &format!("/admin/config/virtual_models/{id}"),
            scope,
            Some(&hash),
            Some(vm.clone()),
        )
        .await
    }

    /// A group of `account` with one key; returns the key's secret.
    async fn key_of(&self, account: &str) -> String {
        let (status, group) = self
            .admin(
                Method::POST,
                "/admin/groups",
                None,
                None,
                Some(json!({ "name": account, "account_ref": account })),
            )
            .await;
        assert_eq!(status, 201, "{group}");
        let (status, key) = self
            .admin(
                Method::POST,
                "/admin/keys",
                None,
                None,
                Some(json!({ "name": "k", "group_id": group["id"] })),
            )
            .await;
        assert_eq!(status, 201, "{key}");
        key["key"].as_str().unwrap().to_owned()
    }

    /// A key with no group (the platform operator's).
    async fn loose_key(&self) -> String {
        let (status, key) = self
            .admin(
                Method::POST,
                "/admin/keys",
                None,
                None,
                Some(json!({ "name": "operator" })),
            )
            .await;
        assert_eq!(status, 201, "{key}");
        key["key"].as_str().unwrap().to_owned()
    }

    /// Chat through `model` with `key`: `(status, model_used, route, body)`.
    async fn chat(&self, key: &str, model: &str) -> (u16, String, String, Value) {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(key)
            .json(&json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }))
            .send()
            .await
            .unwrap();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let (used, route) = (header("x-lumen-model-used"), header("x-lumen-route"));
        let status = resp.status().as_u16();
        (
            status,
            used,
            route,
            resp.json().await.unwrap_or(Value::Null),
        )
    }

    /// `GET /v1/models` ids for `key`, with their `owned_by`.
    async fn models(&self, key: &str) -> Vec<(String, String)> {
        let body: Value = self
            .client
            .get(format!("{}/v1/models", self.base))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["id"].as_str().unwrap().to_owned(),
                    m["owned_by"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// Wait until the hot reload applied a platform change.
    async fn wait_for(&self, what: &str, done: impl Fn(&ResilienceRuntime) -> bool) {
        for _ in 0..200 {
            if done(&self.resilience) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }
}

fn single(id: &str, target: &str) -> Value {
    json!({ "id": id, "capability": "chat", "strategy": "single", "targets": [{ "model": target }] })
}

fn ids(list: &Value) -> Vec<String> {
    list["virtual_models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn two_accounts_are_isolated() {
    let h = spawn().await;
    let (key_a, key_b, loose) = (h.key_of(A).await, h.key_of(B).await, h.loose_key().await);

    let (status, body) = h.put(Some(A), &single("team/fast", "p/chat-a")).await;
    assert_eq!(status, 204, "{body}");

    // The admin views are per account; the platform list is untouched.
    assert_eq!(ids(&h.list(Some(A)).await), ["team/fast"]);
    assert_eq!(ids(&h.list(Some(B)).await), Vec::<String>::new());
    assert_eq!(ids(&h.list(None).await), ["lumen/chat"]);
    let (status, body) = h
        .admin(
            Method::GET,
            "/admin/config/virtual_models/team%2Ffast",
            Some(B),
            None,
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("LM-1003"))
    );
    let (status, got) = h
        .admin(
            Method::GET,
            "/admin/config/virtual_models/team%2Ffast",
            Some(A),
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(got["targets"][0]["model"], "p/chat-a");
    assert!(got["hash"].is_string());

    // Only A's keys resolve it.
    let (status, used, route, body) = h.chat(&key_a, "team/fast").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(used, "p/chat-a");
    assert_eq!(route, "team/fast>p/chat-a");
    for key in [&key_b, &loose] {
        let (status, _, _, body) = h.chat(key, "team/fast").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "LM-2001");
    }

    // B may use the same id for its own model, and each key gets its own.
    let (status, body) = h.put(Some(B), &single("team/fast", "p/chat-b")).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(h.chat(&key_a, "team/fast").await.1, "p/chat-a");
    assert_eq!(h.chat(&key_b, "team/fast").await.1, "p/chat-b");

    // A model may never target another account's model: it is unknown.
    let (status, body) = h.put(Some(B), &single("team/only-b", "p/chat-b")).await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = h.put(Some(A), &single("team/steal", "team/only-b")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "LM-1001");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("team/only-b"));

    // Account ids never become Prometheus label values.
    let metrics = h
        .client
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let counter: Vec<&str> = metrics
        .lines()
        .filter(|l| l.starts_with("lumen_virtual_model_requests_total{"))
        .collect();
    assert!(
        counter
            .iter()
            .any(|l| l.contains(r#"virtual_model="account""#)),
        "{counter:?}"
    );
    assert!(!metrics.contains("team/fast"), "{counter:?}");
}

#[tokio::test]
async fn if_match_is_per_account_and_never_moves_the_platform_hash() {
    let h = spawn().await;
    let platform = h.hash(None).await;
    let (hash_a, hash_b) = (h.hash(Some(A)).await, h.hash(Some(B)).await);
    assert_ne!(hash_a, platform);

    let path = "/admin/config/virtual_models/team%2Fone";
    let vm = single("team/one", "p/chat-a");
    // Missing If-Match is LM-1001, as for the platform.
    let (status, body) = h
        .admin(Method::PUT, path, Some(A), None, Some(vm.clone()))
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("LM-1001"))
    );
    // The platform hash is not an account version.
    let (status, body) = h
        .admin(
            Method::PUT,
            path,
            Some(A),
            Some(&platform),
            Some(vm.clone()),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (412, Some("LM-1004"))
    );

    let (status, body) = h
        .admin(Method::PUT, path, Some(A), Some(&hash_a), Some(vm.clone()))
        .await;
    assert_eq!(status, 204, "{body}");
    let new_a = h.hash(Some(A)).await;
    assert_ne!(new_a, hash_a);
    // A stale account version is refused...
    let (status, body) = h
        .admin(Method::PUT, path, Some(A), Some(&hash_a), Some(vm.clone()))
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (412, Some("LM-1004"))
    );
    let (status, body) = h
        .admin(Method::DELETE, path, Some(A), Some(&hash_a), None)
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (412, Some("LM-1004"))
    );
    // ...while B's version and the platform's did not move.
    assert_eq!(h.hash(Some(B)).await, hash_b);
    assert_eq!(h.hash(None).await, platform);
    let (status, body) = h
        .admin(Method::PUT, path, Some(B), Some(&hash_b), Some(vm.clone()))
        .await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(h.hash(Some(A)).await, new_a);

    // The platform's own writes keep their own version too.
    let (status, body) = h.put(None, &single("lumen/other", "p/chat-b")).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(h.hash(Some(A)).await, new_a);

    // Delete with the current version; the GET is then a 404.
    let (status, body) = h
        .admin(Method::DELETE, path, Some(A), Some(&new_a), None)
        .await;
    assert_eq!(status, 204, "{body}");
    let (status, _) = h.admin(Method::GET, path, Some(A), None, None).await;
    assert_eq!(status, 404);

    // A malformed account header is LM-1001, never "unscoped".
    let (status, body) = h
        .admin(
            Method::GET,
            "/admin/config/virtual_models",
            Some("nope"),
            None,
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("LM-1001"))
    );
}

#[tokio::test]
async fn platform_ids_are_refused_then_shadowed_when_the_platform_adds_one() {
    let h = spawn().await;
    let (key_a, key_b) = (h.key_of(A).await, h.key_of(B).await);

    // Saving under a current platform id is refused.
    for id in ["lumen/chat", "p/chat-a"] {
        let (status, body) = h.put(Some(A), &single(id, "p/chat-b")).await;
        assert_eq!(status, 400, "{id}: {body}");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("platform"));
    }

    // The platform adding an id an account already uses is never blocked.
    let (status, body) = h.put(Some(A), &single("team/fast", "p/chat-b")).await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = h.put(None, &single("team/fast", "p/chat-a")).await;
    assert_eq!(status, 204, "{body}");
    h.wait_for("the platform team/fast", |r| r.is_platform_id("team/fast"))
        .await;

    // A's keys keep A's model; every other key gets the platform's.
    assert_eq!(h.chat(&key_a, "team/fast").await.1, "p/chat-b");
    assert_eq!(h.chat(&key_b, "team/fast").await.1, "p/chat-a");
    let list = h.list(Some(A)).await;
    let warning = list["virtual_models"][0]["warnings"][0].as_str().unwrap();
    assert!(warning.contains("platform"), "{warning}");
    // A key of A lists the id once, as A's.
    let models = h.models(&key_a).await;
    let fast: Vec<_> = models.iter().filter(|(id, _)| id == "team/fast").collect();
    assert_eq!(fast, [&("team/fast".to_owned(), "account".to_owned())]);
    let models = h.models(&key_b).await;
    assert!(models.contains(&("team/fast".to_owned(), "lumen".to_owned())));

    // A shadowing model is still saved strictly: re-saving it is refused,
    // while A's other models stay editable.
    let (status, _) = h.put(Some(A), &single("team/fast", "p/chat-a")).await;
    assert_eq!(status, 400);
    let (status, body) = h.put(Some(A), &single("team/other", "p/chat-a")).await;
    assert_eq!(status, 204, "{body}");
}

#[tokio::test]
async fn platform_deletes_are_never_blocked_and_dependents_skip_the_leaf() {
    let h = spawn().await;
    let key_a = h.key_of(A).await;
    let fallback = json!({
        "id": "team/fb", "capability": "chat", "strategy": "fallback",
        "targets": [{ "model": "lumen/chat" }, { "model": "p/chat-b" }]
    });
    let (status, body) = h.put(Some(A), &fallback).await;
    assert_eq!(status, 204, "{body}");
    let (status, used, route, _) = h.chat(&key_a, "team/fb").await;
    assert_eq!((status, used.as_str()), (200, "p/chat-a"));
    assert_eq!(route, "team/fb>lumen/chat>p/chat-a");

    // The plan shows the platform model as a reference, never its preset.
    let (status, plan) = h
        .admin(
            Method::GET,
            "/admin/config/virtual_models/team%2Ffb/plan",
            Some(A),
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["plan"]["targets"][0]["platform"], true);
    assert!(!plan.to_string().contains(PLATFORM_SECRET), "{plan}");

    // Staff delete the platform model the team model targets.
    let platform = h.hash(None).await;
    let (status, body) = h
        .admin(
            Method::DELETE,
            "/admin/config/virtual_models/lumen%2Fchat",
            None,
            Some(&platform),
            None,
        )
        .await;
    assert_eq!(status, 204, "{body}");
    h.wait_for("the platform delete", |r| !r.is_platform_id("lumen/chat"))
        .await;

    // The missing leaf is skipped at request time and reported.
    let (status, used, route, body) = h.chat(&key_a, "team/fb").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(used, "p/chat-b");
    assert_eq!(route, "team/fb>p/chat-b");
    let list = h.list(Some(A)).await;
    let warning = list["virtual_models"][0]["warnings"][0].as_str().unwrap();
    assert!(warning.contains("lumen/chat"), "{warning}");
    let (_, plan) = h
        .admin(
            Method::GET,
            "/admin/config/virtual_models/team%2Ffb/plan",
            Some(A),
            None,
            None,
        )
        .await;
    assert_eq!(plan["plan"]["targets"][0]["missing"], true);
    assert!(plan["warnings"][0].as_str().unwrap().contains("lumen/chat"));

    // The account can still edit its other models; a new reference to the
    // deleted model is refused.
    let (status, body) = h.put(Some(A), &single("team/other", "p/chat-a")).await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = h.put(Some(A), &single("team/bad", "lumen/chat")).await;
    assert_eq!(status, 400, "{body}");

    // Staff can also delete a whole provider an account model targets.
    let platform = h.hash(None).await;
    let (status, body) = h
        .admin(
            Method::DELETE,
            "/admin/config/providers/p",
            None,
            Some(&platform),
            None,
        )
        .await;
    assert_eq!(status, 204, "{body}");
    h.wait_for("the provider delete", |r| !r.is_platform_id("p/chat-b"))
        .await;
    let (status, _, _, body) = h.chat(&key_a, "team/fb").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "LM-2001");
}

#[tokio::test]
async fn v1_models_lists_account_models_only_for_their_keys() {
    let h = spawn().await;
    let (key_a, key_b, loose) = (h.key_of(A).await, h.key_of(B).await, h.loose_key().await);
    let (status, _) = h.put(Some(A), &single("team/listed", "p/chat-a")).await;
    assert_eq!(status, 204);
    let mut hidden = single("team/hidden", "p/chat-a");
    hidden["listed"] = json!(false);
    let (status, _) = h.put(Some(A), &hidden).await;
    assert_eq!(status, 204);

    let a = h.models(&key_a).await;
    assert!(a.contains(&("team/listed".to_owned(), "account".to_owned())));
    assert!(a.contains(&("lumen/chat".to_owned(), "lumen".to_owned())));
    assert!(!a.iter().any(|(id, _)| id == "team/hidden"));
    for key in [&key_b, &loose] {
        assert!(!h
            .models(key)
            .await
            .iter()
            .any(|(id, _)| id.starts_with("team/")));
    }

    let one = |key: &str| {
        h.client
            .get(format!("{}/v1/models/team%2Flisted", h.base))
            .bearer_auth(key.to_owned())
            .send()
    };
    let resp = one(&key_a).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        (body["virtual"].as_bool(), body["owned_by"].as_str()),
        (Some(true), Some("account"))
    );
    assert_eq!(one(&key_b).await.unwrap().status(), 404);
    // Unlisted stays callable but is not retrievable, as for the platform.
    assert_eq!(h.chat(&key_a, "team/hidden").await.0, 200);
}

#[tokio::test]
async fn deletes_and_limits_are_checked_per_account() {
    let h = spawn().await;
    let (status, _) = h.put(Some(A), &single("team/inner", "p/chat-a")).await;
    assert_eq!(status, 204);
    let (status, _) = h.put(Some(A), &single("team/outer", "team/inner")).await;
    assert_eq!(status, 204);
    let hash = h.hash(Some(A)).await;
    let (status, body) = h
        .admin(
            Method::DELETE,
            "/admin/config/virtual_models/team%2Finner",
            Some(A),
            Some(&hash),
            None,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("team/outer"));
    let (status, _) = h
        .admin(
            Method::DELETE,
            "/admin/config/virtual_models/team%2Fnope",
            Some(A),
            Some(&hash),
            None,
        )
        .await;
    assert_eq!(status, 404);
    // A change of capability that breaks a dependent is refused too.
    let embed = json!({ "id": "team/inner", "capability": "embed", "strategy": "single",
                        "targets": [{ "model": "p/chat-a" }] });
    let (status, _) = h.put(Some(A), &embed).await;
    assert_eq!(status, 400);

    // At most 32 models per account (two are already there).
    for i in 0..30 {
        let (status, body) = h
            .put(Some(A), &single(&format!("team/m{i}"), "p/chat-a"))
            .await;
        assert_eq!(status, 204, "{body}");
    }
    let (status, body) = h
        .put(Some(A), &single("team/one-too-many", "p/chat-a"))
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("32"));
    // The limit is per account.
    let (status, _) = h.put(Some(B), &single("team/m0", "p/chat-a")).await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn another_process_picks_up_account_writes_through_the_refresh() {
    let h = spawn().await;
    let (status, _) = h.put(Some(A), &single("team/fast", "p/chat-a")).await;
    assert_eq!(status, 204);

    // A second process over the same database, with the same platform.
    let other = ResilienceRuntime::defaults();
    assert!(other.account_models(A).is_none());
    lumen_server::account_models::refresh(&h.store, &other).await;
    let models = other.account_models(A).unwrap();
    assert_eq!(models.hash, h.hash(Some(A)).await);
    assert!(other.account_generation() >= 1);
    // Compiled over that process's own (empty) platform: the target is a
    // skipped leaf there, never an error.
    assert_eq!(models.table.problems("team/fast").len(), 1);

    // A later write is seen; an older row never overwrites a newer one.
    let (status, _) = h.put(Some(A), &single("team/second", "p/chat-a")).await;
    assert_eq!(status, 204);
    lumen_server::account_models::refresh(&h.store, &other).await;
    assert_eq!(other.account_models(A).unwrap().configs.len(), 2);
    let stale = (A.to_owned(), Vec::new(), "old".to_owned(), 1);
    other.install_accounts(vec![stale], true);
    assert_eq!(other.account_models(A).unwrap().configs.len(), 2);
}

#[tokio::test]
async fn an_admin_write_never_hides_an_older_write_from_the_refresher() {
    let h = spawn().await;
    // Another process writes account B (generation 1) that this process has
    // not loaded yet; this process then serves a write for A (generation 2).
    let text = r#"[{"id":"team/b","capability":"chat","strategy":"single","targets":[{"model":"p/chat-a"}]}]"#;
    let hash = lumen_server::account_models::document_hash(text);
    let empty = lumen_server::account_models::empty_hash();
    h.store
        .save_account_virtual_models(B, text, &hash, &empty, &empty)
        .await
        .unwrap();
    let (status, _) = h.put(Some(A), &single("team/a", "p/chat-a")).await;
    assert_eq!(status, 204);
    assert!(h.resilience.account_models(A).is_some());
    assert!(h.resilience.account_models(B).is_none());
    // The next refresh still loads B's older row.
    lumen_server::account_models::refresh(&h.store, &h.resilience).await;
    assert_eq!(h.resilience.account_models(B).unwrap().hash, hash);
    assert_eq!(h.resilience.account_generation(), 2);
}
