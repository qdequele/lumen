//! End-to-end tests for budget groups (ADR 009): the master-key-gated
//! `/admin/groups` CRUD surface, group membership on `/admin/keys`, and
//! shared-pool enforcement through a live gateway - two keys draining one
//! pool, group-scoped 402s decided before any upstream call, and a group
//! PATCH taking effect immediately with no restart. The upstream is
//! wiremock; LUMEN sits in front with auth enabled and an in-memory SQLite
//! store.

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

    /// GET `/admin/groups` (optionally with a query string) as JSON.
    async fn list_groups(&self, query: &str) -> Value {
        self.client
            .get(format!("{}/admin/groups{query}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("list groups")
            .json()
            .await
            .expect("json")
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

    /// PATCH `/admin/keys/{id}` with the master key.
    async fn patch_key(&self, id: &str, body: &Value) -> reqwest::Response {
        self.client
            .patch(format!("{}/admin/keys/{id}", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("patch key")
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
async fn spawn_groups(registry: Arc<Registry>) -> Harness {
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
async fn admin_groups_require_the_master_key() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    // No Authorization header at all.
    let no_auth = h
        .client
        .get(format!("{}/admin/groups", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(no_auth.status(), 401);

    // A wrong master key.
    let wrong = h
        .client
        .post(format!("{}/admin/groups", h.base))
        .bearer_auth("b".repeat(64))
        .json(&json!({ "name": "acme" }))
        .send()
        .await
        .expect("send");
    assert_eq!(wrong.status(), 401);

    // A virtual key is NOT an admin key.
    let created = h.create_key(&json!({ "name": "vkey" })).await;
    assert_eq!(created.status(), 201);
    let vkey = created.json::<Value>().await.expect("json")["key"]
        .as_str()
        .expect("key")
        .to_owned();
    let with_vkey = h
        .client
        .get(format!("{}/admin/groups", h.base))
        .bearer_auth(&vkey)
        .send()
        .await
        .expect("send");
    assert_eq!(with_vkey.status(), 401);
}

// ---- CRUD -------------------------------------------------------------------

#[tokio::test]
async fn admin_group_create_list_patch_delete_round_trip() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    // Create: 201 with the record - a group has no secret to reveal.
    let created = h
        .create_group(&json!({ "name": "acme", "budget_max": 25.0 }))
        .await;
    assert_eq!(created.status(), 201);
    let body: Value = created.json().await.expect("json");
    let id = body["id"].as_str().expect("id").to_owned();
    assert_eq!(body["name"], "acme");
    assert_eq!(body["budget_max"], 25.0);
    assert_eq!(body["budget_spent"], 0.0);
    assert!(body["deleted_at"].is_null());

    // The list shows it.
    let list = h.list_groups("").await;
    let groups = list.as_array().expect("array");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["id"], *id);

    // Patch: adjust the budget, name untouched.
    let patched = h
        .client
        .patch(format!("{}/admin/groups/{id}", h.base))
        .bearer_auth(master())
        .json(&json!({ "budget_max": 50.0 }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patched.status(), 200);
    let patched_body: Value = patched.json().await.expect("json");
    assert_eq!(patched_body["budget_max"], 50.0);
    assert_eq!(patched_body["name"], "acme");

    // Delete: 204, hidden by default, visible as a tombstone on request.
    let deleted = h
        .client
        .delete(format!("{}/admin/groups/{id}", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);

    let default_list = h.list_groups("").await;
    assert!(
        default_list.as_array().expect("array").is_empty(),
        "deleted group must not appear by default: {default_list}"
    );
    let all = h.list_groups("?include_deleted=true").await;
    let tombstone = all
        .as_array()
        .expect("array")
        .iter()
        .find(|g| g["id"] == *id)
        .expect("tombstone listed");
    assert!(tombstone["deleted_at"].is_i64());
}

#[tokio::test]
async fn admin_group_create_with_a_blank_name_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    for name in ["", "   "] {
        let resp = h.create_group(&json!({ "name": name })).await;
        assert_eq!(resp.status(), 400, "name {name:?} must be refused");
        let body: Value = resp.json().await.expect("json");
        assert_eq!(body["error"]["code"], "LM-1001", "name {name:?}");
    }

    // Nothing was created.
    let list = h.list_groups("").await;
    assert!(list.as_array().expect("array").is_empty());
}

#[tokio::test]
async fn admin_group_patch_and_delete_of_unknown_or_deleted_id_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    // Unknown ids.
    let patch = h
        .client
        .patch(format!("{}/admin/groups/nope", h.base))
        .bearer_auth(master())
        .json(&json!({ "budget_max": 1.0 }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patch.status(), 400);
    let patch_body: Value = patch.json().await.expect("json");
    assert_eq!(patch_body["error"]["code"], "LM-1001");

    let del = h
        .client
        .delete(format!("{}/admin/groups/nope", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status(), 400);
    let del_body: Value = del.json().await.expect("json");
    assert_eq!(del_body["error"]["code"], "LM-1001");

    // Tombstones behave like unknown ids.
    let created = h.create_group(&json!({ "name": "ghost" })).await;
    assert_eq!(created.status(), 201);
    let id = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let deleted = h
        .client
        .delete(format!("{}/admin/groups/{id}", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);

    let patch_tombstone = h
        .client
        .patch(format!("{}/admin/groups/{id}", h.base))
        .bearer_auth(master())
        .json(&json!({ "budget_max": 1.0 }))
        .send()
        .await
        .expect("patch tombstone");
    assert_eq!(patch_tombstone.status(), 400);
    let body: Value = patch_tombstone.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-1001");

    let delete_again = h
        .client
        .delete(format!("{}/admin/groups/{id}", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("delete again");
    assert_eq!(delete_again.status(), 400);
    let body: Value = delete_again.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-1001");
}

#[tokio::test]
async fn admin_group_delete_with_active_member_keys_is_400_and_names_the_count() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    let created = h.create_group(&json!({ "name": "busy" })).await;
    assert_eq!(created.status(), 201);
    let gid = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    for name in ["m1", "m2"] {
        let key = h
            .create_key(&json!({ "name": name, "group_id": gid }))
            .await;
        assert_eq!(key.status(), 201);
    }

    let refused = h
        .client
        .delete(format!("{}/admin/groups/{gid}", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("delete");
    assert_eq!(refused.status(), 400);
    let body: Value = refused.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-1001");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(
        message.contains('2'),
        "the refusal must name the member count, got: {message}"
    );

    // The refused delete wrote nothing: the group is still active.
    let list = h.list_groups("").await;
    assert_eq!(list.as_array().expect("array").len(), 1);
}

// ---- Key membership ----------------------------------------------------------

#[tokio::test]
async fn admin_key_create_with_an_unknown_group_is_400_lm1001() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    let resp = h
        .create_key(&json!({ "name": "orphan", "group_id": "nope" }))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-1001");

    // Refused before any write: no key row landed.
    assert!(h.store.list_keys(false).await.expect("list").is_empty());
}

#[tokio::test]
async fn admin_key_create_with_a_group_id_exposes_the_membership() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    let created = h.create_group(&json!({ "name": "home" })).await;
    assert_eq!(created.status(), 201);
    let gid = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let key = h
        .create_key(&json!({ "name": "member", "group_id": gid }))
        .await;
    assert_eq!(key.status(), 201);
    let key_body: Value = key.json().await.expect("json");
    assert_eq!(key_body["group_id"], *gid);

    // The list exposes the membership too.
    let list: Value = h
        .client
        .get(format!("{}/admin/keys", h.base))
        .bearer_auth(master())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    assert_eq!(list[0]["group_id"], *gid);
}

#[tokio::test]
async fn admin_key_patch_group_id_joins_keeps_and_null_leaves() {
    let upstream = MockServer::start().await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    let g1 = h.create_group(&json!({ "name": "g1" })).await;
    let g1_id = g1.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let g2 = h.create_group(&json!({ "name": "g2" })).await;
    let g2_id = g2.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let created = h
        .create_key(&json!({ "name": "mover", "group_id": g1_id }))
        .await;
    assert_eq!(created.status(), 201);
    let key_id = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // A string joins that group.
    let moved = h.patch_key(&key_id, &json!({ "group_id": g2_id })).await;
    assert_eq!(moved.status(), 200);
    let moved_body: Value = moved.json().await.expect("json");
    assert_eq!(moved_body["group_id"], *g2_id);

    // An ABSENT group_id leaves the membership untouched (tri-state).
    let renamed = h.patch_key(&key_id, &json!({ "name": "renamed" })).await;
    assert_eq!(renamed.status(), 200);
    let renamed_body: Value = renamed.json().await.expect("json");
    assert_eq!(renamed_body["group_id"], *g2_id);

    // An explicit null leaves the group.
    let left = h.patch_key(&key_id, &json!({ "group_id": null })).await;
    assert_eq!(left.status(), 200);
    let left_body: Value = left.json().await.expect("json");
    assert!(left_body["group_id"].is_null());

    // An unknown group is refused.
    let refused = h.patch_key(&key_id, &json!({ "group_id": "nope" })).await;
    assert_eq!(refused.status(), 400);
    let refused_body: Value = refused.json().await.expect("json");
    assert_eq!(refused_body["error"]["code"], "LM-1001");
}

// ---- Shared-pool enforcement, end to end --------------------------------------

#[tokio::test]
async fn group_pool_exhaustion_is_402_group_scoped_on_either_key_and_a_patch_reopens_it() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let h = spawn_groups(chat_registry(&upstream.uri())).await;

    // A $100 pool shared by two keys with NO budgets of their own.
    let created = h
        .create_group(&json!({ "name": "prepaid", "budget_max": 100.0 }))
        .await;
    assert_eq!(created.status(), 201);
    let gid = created.json::<Value>().await.expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let mut keys = Vec::new();
    for name in ["project-a", "project-b"] {
        let resp = h
            .create_key(&json!({ "name": name, "group_id": gid }))
            .await;
        assert_eq!(resp.status(), 201);
        keys.push(
            resp.json::<Value>().await.expect("json")["key"]
                .as_str()
                .expect("key")
                .to_owned(),
        );
    }

    // Each call settles at $46 (12 + 34 upstream tokens at $1): two calls
    // drain $92 of the $100 pool, leaving less than the next $10 estimate.
    assert_eq!(h.chat(&keys[0]).await.status(), 200);
    assert_eq!(h.chat(&keys[1]).await.status(), 200);

    // EITHER key is now refused, with the group-scoped LM-4001 message...
    for key in &keys {
        let refused = h.chat(key).await;
        assert_eq!(refused.status(), 402);
        let body: Value = refused.json().await.expect("json");
        assert_eq!(body["error"]["code"], "LM-4001");
        assert_eq!(
            body["error"]["message"],
            "budget exceeded for this key's group"
        );
    }
    // ...and the refusals never reached the provider: exactly the two
    // successful calls made it upstream.
    assert_eq!(upstream.received_requests().await.expect("reqs").len(), 2);

    // Top up the pool. The next request is admitted IMMEDIATELY, no restart.
    let patched = h
        .client
        .patch(format!("{}/admin/groups/{gid}", h.base))
        .bearer_auth(master())
        .json(&json!({ "budget_max": 200.0 }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patched.status(), 200);

    assert_eq!(h.chat(&keys[0]).await.status(), 200);
    assert_eq!(upstream.received_requests().await.expect("reqs").len(), 3);
}
