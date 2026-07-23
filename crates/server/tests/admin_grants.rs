//! End-to-end tests for atomic budget grants: the master-key-gated
//! `POST /admin/keys/{id}/grant` and `POST /admin/groups/{id}/grant` routes.
//! A prepaid-credits control plane tops budgets up while traffic flows, and a
//! read-modify-write PATCH would race concurrent top-ups - so a grant is an
//! atomic increment of `budget_max` on BOTH the DB row and the live
//! in-memory entry, effective on the very next request with no reload. The
//! upstream is wiremock; LUMEN sits in front with auth enabled and an
//! in-memory SQLite store.

// Exact float literals stored and read back unchanged through SQLite REAL
// columns - strict equality is the correct assertion here.
#![allow(clippy::float_cmp)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::auth::AuthRuntime;
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
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

/// A chat-only OpenAI registry over the wiremock upstream.
fn chat_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gpt".to_owned(),
            upstream_id: "gpt-4o-2024-08-06".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    )
}

/// $1 per token, input and output: the budget arithmetic below is 1:1 with
/// token counts. The fixed chat call ("hi" + max_tokens 5) estimates to $10
/// pre-call; the mocked upstream usage (12 in / 34 out) settles each
/// successful call at $46.
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
    "#;
    let config: Config = Figment::new()
        .merge(Toml::string(toml))
        .extract()
        .expect("valid pricing config");
    CostTable::from_config(&config)
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

struct Harness {
    base: String,
    store: KeyStore,
    client: reqwest::Client,
}

impl Harness {
    /// POST `/admin/groups` with the master key.
    async fn create_group(&self, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/groups", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("create group")
    }

    /// POST `/admin/keys` with the master key.
    async fn create_key(&self, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("create key")
    }

    /// POST `/admin/keys/{id}/grant` with the master key.
    async fn grant_key(&self, id: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys/{id}/grant", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("grant key")
    }

    /// POST `/admin/groups/{id}/grant` with the master key.
    async fn grant_group(&self, id: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/groups/{id}/grant", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("grant group")
    }

    /// POST a key grant with a raw JSON body string - for bodies `json!`
    /// cannot build, like the overflowing literal `1e999`.
    async fn grant_key_raw(&self, id: &str, body: &'static str) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys/{id}/grant", self.base))
            .bearer_auth(master())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("grant key raw")
    }

    /// POST a group grant with a raw JSON body string.
    async fn grant_group_raw(&self, id: &str, body: &'static str) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/groups/{id}/grant", self.base))
            .bearer_auth(master())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("grant group raw")
    }

    /// DELETE `/admin/keys/{id}` with the master key.
    async fn delete_key(&self, id: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/admin/keys/{id}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("delete key")
    }

    /// DELETE `/admin/groups/{id}` with the master key.
    async fn delete_group(&self, id: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/admin/groups/{id}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("delete group")
    }

    /// One fixed chat call: "hi" with max_tokens 5, a $10 pre-call estimate
    /// at the test pricing.
    async fn chat(&self, key: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(key)
            .json(&json!({
                "model": "gpt",
                "messages": [{ "role": "user", "content": "hi" }],
                "max_tokens": 5
            }))
            .send()
            .await
            .expect("chat request")
    }
}

/// Spawn a full auth-enabled gateway with the dollar price table attached
/// (so budget admission can refuse). Groups load BEFORE keys, exactly as
/// boot does (ADR 009 §4: keys resolve group pointers).
async fn spawn_gateway(registry: Arc<Registry>) -> Harness {
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
        .with_pricing(dollar_pricing())
        .with_auth(runtime)
        .with_usage(logger);
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        store,
        client: reqwest::Client::new(),
    }
}

// ---- Gating -----------------------------------------------------------------

#[tokio::test]
async fn admin_grant_routes_require_the_master_key() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    for route in ["admin/keys/any/grant", "admin/groups/any/grant"] {
        // No Authorization header at all.
        let no_auth = h
            .client
            .post(format!("{}/{route}", h.base))
            .json(&json!({ "amount": 5.0 }))
            .send()
            .await
            .expect("send");
        assert_eq!(no_auth.status(), 401, "{route} without a token");

        // A wrong master key.
        let wrong = h
            .client
            .post(format!("{}/{route}", h.base))
            .bearer_auth("b".repeat(64))
            .json(&json!({ "amount": 5.0 }))
            .send()
            .await
            .expect("send");
        assert_eq!(wrong.status(), 401, "{route} with a wrong token");
    }
}

// ---- Happy paths ------------------------------------------------------------

#[tokio::test]
async fn admin_key_grant_raises_the_cap_and_returns_the_record() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    let created = h
        .create_key(&json!({ "name": "prepaid-key", "budget_max": 10.0 }))
        .await;
    assert_eq!(created.status(), 201);
    let id = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let granted = h.grant_key(&id, &json!({ "amount": 5.0 })).await;
    assert_eq!(granted.status(), 200);
    let body: Value = granted.json().await.expect("json");
    assert_eq!(body["id"], *id);
    assert_eq!(body["budget_max"], 15.0, "10 + 5 = 15");
    assert_eq!(body["budget_spent"], 0.0, "a grant never touches spend");
    // The record only: unlike create/rotate, a grant mints no secret.
    assert!(
        body.get("key").is_none(),
        "grant response must never carry a plaintext key: {body}"
    );
}

#[tokio::test]
async fn admin_group_grant_raises_the_cap_and_returns_the_record() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    let created = h
        .create_group(&json!({ "name": "prepaid-pool", "budget_max": 25.0 }))
        .await;
    assert_eq!(created.status(), 201);
    let id = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let granted = h.grant_group(&id, &json!({ "amount": 5.0 })).await;
    assert_eq!(granted.status(), 200);
    let body: Value = granted.json().await.expect("json");
    assert_eq!(body["id"], *id);
    assert_eq!(body["budget_max"], 30.0, "25 + 5 = 30");
    assert_eq!(body["budget_spent"], 0.0, "a grant never touches spend");
}

// ---- The prepaid-credits flow, end to end -----------------------------------

#[tokio::test]
async fn group_grant_reopens_a_drained_pool_immediately_with_no_reload() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    // One prepaid $100 pool, one project key with no budget of its own.
    let created = h
        .create_group(&json!({ "name": "prepaid", "budget_max": 100.0 }))
        .await;
    assert_eq!(created.status(), 201);
    let gid = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let key_resp = h
        .create_key(&json!({ "name": "project", "group_id": gid }))
        .await;
    assert_eq!(key_resp.status(), 201);
    let key = key_resp.json::<Value>().await.expect("json")["key"]
        .as_str()
        .expect("key")
        .to_owned();

    // Each call settles at $46 (12 + 34 upstream tokens at $1): two calls
    // drain $92 of the $100 pool, leaving less than the next $10 estimate.
    assert_eq!(h.chat(&key).await.status(), 200);
    assert_eq!(h.chat(&key).await.status(), 200);

    let refused = h.chat(&key).await;
    assert_eq!(refused.status(), 402);
    let refusal: Value = refused.json().await.expect("json");
    assert_eq!(refusal["error"]["code"], "LM-4001");
    assert_eq!(
        refusal["error"]["message"],
        "budget exceeded for this key's group"
    );
    // The refusal was decided in memory: only the two successes went upstream.
    assert_eq!(upstream.received_requests().await.expect("reqs").len(), 2);

    // The control plane tops the pool up - one atomic grant, and NO reload
    // call anywhere in this test.
    let granted = h.grant_group(&gid, &json!({ "amount": 100.0 })).await;
    assert_eq!(granted.status(), 200);
    let granted_body: Value = granted.json().await.expect("json");
    assert_eq!(granted_body["budget_max"], 200.0, "100 + 100 = 200");

    // The SAME request is admitted IMMEDIATELY and reaches the provider.
    assert_eq!(h.chat(&key).await.status(), 200);
    assert_eq!(upstream.received_requests().await.expect("reqs").len(), 3);
}

// ---- Invalid grants ----------------------------------------------------------

#[tokio::test]
async fn admin_grant_with_a_non_positive_non_finite_or_missing_amount_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    let group = h
        .create_group(&json!({ "name": "capped-pool", "budget_max": 25.0 }))
        .await;
    assert_eq!(group.status(), 201);
    let gid = group.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let key = h
        .create_key(&json!({ "name": "capped-key", "budget_max": 10.0 }))
        .await;
    assert_eq!(key.status(), 201);
    let kid = key.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // Zero, negative, and a missing amount field.
    for body in [
        json!({ "amount": 0.0 }),
        json!({ "amount": -5.0 }),
        json!({}),
    ] {
        let key_resp = h.grant_key(&kid, &body).await;
        assert_eq!(key_resp.status(), 400, "key grant body {body}");
        let key_err: Value = key_resp.json().await.expect("json");
        assert_eq!(key_err["error"]["code"], "LM-1001", "key grant body {body}");

        let group_resp = h.grant_group(&gid, &body).await;
        assert_eq!(group_resp.status(), 400, "group grant body {body}");
        let group_err: Value = group_resp.json().await.expect("json");
        assert_eq!(
            group_err["error"]["code"], "LM-1001",
            "group grant body {body}"
        );
    }

    // 1e999 overflows f64: serde_json rejects the literal at parse time
    // (LM-1001 via the extractor); the handler's finite check is
    // belt-and-braces should a future serde version saturate to +inf
    // instead. Raw body - `json!` cannot carry the literal.
    let inf_key = h.grant_key_raw(&kid, r#"{"amount": 1e999}"#).await;
    assert_eq!(inf_key.status(), 400);
    let inf_key_err: Value = inf_key.json().await.expect("json");
    assert_eq!(inf_key_err["error"]["code"], "LM-1001");
    let inf_group = h.grant_group_raw(&gid, r#"{"amount": 1e999}"#).await;
    assert_eq!(inf_group.status(), 400);
    let inf_group_err: Value = inf_group.json().await.expect("json");
    assert_eq!(inf_group_err["error"]["code"], "LM-1001");

    // A huge-but-finite amount is refused by the MAX_GRANT_USD bound:
    // repeated grants of ~1.8e308 would sum the DB float to +Inf, which
    // serializes as null and reloads as the UNLIMITED sentinel - the exact
    // accidental-unlimited the grant path promises never to mint.
    for over in [json!({ "amount": 2e12 }), json!({ "amount": f64::MAX })] {
        let key_resp = h.grant_key(&kid, &over).await;
        assert_eq!(key_resp.status(), 400, "key grant body {over}");
        let key_err: Value = key_resp.json().await.expect("json");
        assert_eq!(key_err["error"]["code"], "LM-1001", "key grant body {over}");
        let group_resp = h.grant_group(&gid, &over).await;
        assert_eq!(group_resp.status(), 400, "group grant body {over}");
        let group_err: Value = group_resp.json().await.expect("json");
        assert_eq!(
            group_err["error"]["code"], "LM-1001",
            "group grant body {over}"
        );
    }

    // None of the refusals wrote anything: both caps are unchanged.
    assert_eq!(
        h.store.list_keys(false).await.expect("list keys")[0].budget_max,
        Some(10.0)
    );
    assert_eq!(
        h.store.list_groups(false).await.expect("list groups")[0].budget_max,
        Some(25.0)
    );
}

#[tokio::test]
async fn admin_grant_to_an_unknown_or_deleted_id_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    // Unknown ids.
    let unknown_key = h.grant_key("nope", &json!({ "amount": 5.0 })).await;
    assert_eq!(unknown_key.status(), 400);
    let unknown_key_err: Value = unknown_key.json().await.expect("json");
    assert_eq!(unknown_key_err["error"]["code"], "LM-1001");

    let unknown_group = h.grant_group("nope", &json!({ "amount": 5.0 })).await;
    assert_eq!(unknown_group.status(), 400);
    let unknown_group_err: Value = unknown_group.json().await.expect("json");
    assert_eq!(unknown_group_err["error"]["code"], "LM-1001");

    // Tombstones behave like unknown ids: a deleted key...
    let key = h
        .create_key(&json!({ "name": "doomed-key", "budget_max": 10.0 }))
        .await;
    assert_eq!(key.status(), 201);
    let kid = key.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(h.delete_key(&kid).await.status(), 204);
    let deleted_key = h.grant_key(&kid, &json!({ "amount": 5.0 })).await;
    assert_eq!(deleted_key.status(), 400);
    let deleted_key_err: Value = deleted_key.json().await.expect("json");
    assert_eq!(deleted_key_err["error"]["code"], "LM-1001");

    // ...and a deleted (member-less) group.
    let group = h
        .create_group(&json!({ "name": "doomed-pool", "budget_max": 25.0 }))
        .await;
    assert_eq!(group.status(), 201);
    let gid = group.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(h.delete_group(&gid).await.status(), 204);
    let deleted_group = h.grant_group(&gid, &json!({ "amount": 5.0 })).await;
    assert_eq!(deleted_group.status(), 400);
    let deleted_group_err: Value = deleted_group.json().await.expect("json");
    assert_eq!(deleted_group_err["error"]["code"], "LM-1001");
}

#[tokio::test]
async fn admin_grant_to_an_uncapped_key_or_group_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_gateway(chat_registry(&upstream.uri())).await;

    // Created WITHOUT budget_max: granting to an unlimited budget is
    // meaningless - the operator must PATCH a cap on first, and the error
    // message must say so.
    let key = h.create_key(&json!({ "name": "uncapped-key" })).await;
    assert_eq!(key.status(), 201);
    let kid = key.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let group = h.create_group(&json!({ "name": "uncapped-pool" })).await;
    assert_eq!(group.status(), 201);
    let gid = group.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let key_resp = h.grant_key(&kid, &json!({ "amount": 5.0 })).await;
    assert_eq!(key_resp.status(), 400);
    let key_err: Value = key_resp.json().await.expect("json");
    assert_eq!(key_err["error"]["code"], "LM-1001");
    let key_message = key_err["error"]["message"].as_str().expect("message");
    assert!(
        key_message.contains("no budget cap"),
        "must say there is no budget cap to grant to, got: {key_message}"
    );

    let group_resp = h.grant_group(&gid, &json!({ "amount": 5.0 })).await;
    assert_eq!(group_resp.status(), 400);
    let group_err: Value = group_resp.json().await.expect("json");
    assert_eq!(group_err["error"]["code"], "LM-1001");
    let group_message = group_err["error"]["message"].as_str().expect("message");
    assert!(
        group_message.contains("no budget cap"),
        "must say there is no budget cap to grant to, got: {group_message}"
    );

    // Still uncapped: the refusals wrote nothing.
    assert_eq!(
        h.store.list_keys(false).await.expect("list keys")[0].budget_max,
        None
    );
    assert_eq!(
        h.store.list_groups(false).await.expect("list groups")[0].budget_max,
        None
    );
}
