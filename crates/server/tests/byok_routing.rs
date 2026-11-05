//! Team key pools (BYOK) on the request path: a request whose budget group
//! has a pool for the provider is sent with the team's keys, rotating on a
//! rate limit or a revoked key; an exhausted pool fails `LM-3021` unless the
//! group turned the platform fallback on. Team-key traffic is not billed
//! tokens. Observed through the upstream's `Authorization` headers and the
//! units of the flush batch.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lumen_auth::billing::{BillingPolicy, UsageDelta};
use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewGroup, NewKey};
use lumen_providers::{http, Registry};
use lumen_router::circuit::CircuitState;
use lumen_server::auth::AuthRuntime;
use lumen_server::byok::{self, TeamPools};
use lumen_server::config::Config;
use lumen_server::config_source::ConfigContext;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;
const ADMIN: &str = "byok-admin-token";
const PLATFORM: &str = "sk-platform-key-0000";
const TEAM_1: &str = "sk-team-ONE-secret-1111";
const TEAM_2: &str = "sk-team-TWO-secret-2222";
const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const ACCOUNT_B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";
const ACCOUNT_POOR: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b63";

fn master() -> MasterKey {
    MasterKey::from_env_value(&"b".repeat(64)).expect("master key")
}

/// Two OpenAI-kind providers (one per mock server), priced so platform
/// traffic has a visible cost, and a fallback virtual model a -> b.
fn config_text(a: &str, b: &str, kind_a: &str) -> String {
    format!(
        r#"
        [resilience]
        retry_max_attempts = 1
        retry_base_ms = 10
        retry_max_ms = 20

        [[providers]]
        name = "a"
        kind = "{kind_a}"
        base_url = "{a}"
        [[providers.models]]
        id = "model-a"
        capabilities = ["chat", "embed"]
        cost_per_1m_input = 1000.0
        cost_per_1m_output = 2000.0

        [[providers]]
        name = "b"
        kind = "openai"
        base_url = "{b}"
        [[providers.models]]
        id = "model-b"
        capabilities = ["chat"]
        cost_per_1m_input = 1000.0
        cost_per_1m_output = 2000.0

        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "model-a" }}, {{ model = "model-b" }}]
        "#
    )
}

fn specs(config: &Config) -> Vec<lumen_providers::ProviderSpec> {
    let mut specs = config.provider_specs();
    for spec in &mut specs {
        spec.api_key = Some(PLATFORM.to_owned());
    }
    specs
}

struct Harness {
    base: String,
    runtime: Arc<AuthRuntime>,
    registry: Arc<Registry>,
    pools: Arc<TeamPools>,
    resilience: Arc<ResilienceRuntime>,
    group_a: String,
    group_b: String,
    /// Virtual key of group A (the team with keys).
    key_a: String,
    /// Virtual key of group B (another team).
    key_b: String,
    /// Virtual key with no group.
    key_solo: String,
    /// Virtual key of a group with no credit left.
    key_poor: String,
    group_poor: String,
    config_path: std::path::PathBuf,
    _dir: TempDir,
}

async fn harness(a: &MockServer, b: &MockServer) -> Harness {
    let text = config_text(&a.uri(), &b.uri(), "openai");
    let config = Config::load_text(&text, "test").expect("valid config");
    let dir = TempDir::new().expect("temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, &text).expect("write config");
    let registry = Arc::new(
        Registry::build(
            specs(&config),
            http::build_client(),
            Duration::from_secs(300),
        )
        .expect("registry builds"),
    );

    let store = KeyStore::in_memory().await.expect("store");
    let mut groups = Vec::new();
    for (account, budget) in [
        (ACCOUNT_A, Some(1000.0)),
        (ACCOUNT_B, Some(1000.0)),
        (ACCOUNT_POOR, Some(0.0)),
    ] {
        let group = store
            .create_group(NewGroup {
                name: account.to_owned(),
                budget_max: budget,
                account_ref: Some(account.to_owned()),
            })
            .await
            .expect("group");
        groups.push(group.id);
    }
    let mut keys = Vec::new();
    for (name, group) in [
        ("a", Some(groups[0].clone())),
        ("b", Some(groups[1].clone())),
        ("solo", None),
        ("poor", Some(groups[2].clone())),
    ] {
        let (plain, _) = store
            .create_key(NewKey {
                name: name.to_owned(),
                group_id: group,
                external_ref: Some(format!("lab-{name}")),
                ..NewKey::default()
            })
            .await
            .expect("key");
        keys.push(plain.reveal().to_owned());
    }
    let auth = AuthState::load(
        store.load_groups().await.expect("groups"),
        store.load_auth_entries().await.expect("entries"),
    );
    auth.set_billing(Some(Arc::new(BillingPolicy {
        source: "eu-1".to_owned(),
    })));
    let runtime = Arc::new(AuthRuntime {
        keys: auth,
        store,
        admin_token_hash: hash_key(ADMIN),
        master: Some(master()),
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("tokens");
    let latency = LatencyMetrics::register(&metrics).expect("latency");
    let pools = Arc::new(TeamPools::default());
    let resilience = Arc::new(ResilienceRuntime::from_config(&config, None));
    let state = AppState::new(metrics, Arc::clone(&registry), tokens, latency)
        .with_pricing(CostTable::from_config(&config))
        .with_resilience(Arc::clone(&resilience))
        .with_auth(Arc::clone(&runtime))
        .with_config_context(Arc::new(ConfigContext::file(config_path.clone())))
        .with_team_pools(Arc::clone(&pools));
    let base = common::spawn_state(state, LIMIT).await;
    Harness {
        base,
        runtime,
        registry,
        pools,
        resilience,
        group_a: groups[0].clone(),
        group_b: groups[1].clone(),
        group_poor: groups[2].clone(),
        key_a: keys[0].clone(),
        key_b: keys[1].clone(),
        key_solo: keys[2].clone(),
        key_poor: keys[3].clone(),
        config_path,
        _dir: dir,
    }
}

impl Harness {
    /// Rebuild the in-memory pools from the store (what the reloader does
    /// after every pool write).
    async fn reload(&self) {
        byok::refresh_pools(&self.pools, &self.runtime.store, &master(), &self.registry)
            .await
            .expect("pools load");
    }

    /// Seal a team key into `group`'s pool for `provider` and reload. Returns
    /// the key id.
    async fn add_key(&self, group: &str, provider: &str, secret: &str) -> String {
        let record = self
            .runtime
            .store
            .add_group_provider_key(group, provider, "team", secret, &master())
            .await
            .expect("add key");
        self.reload().await;
        record.id
    }

    async fn set_fallback(&self, group: &str, provider: &str, on: bool) {
        self.runtime
            .store
            .set_group_provider_fallback(group, provider, on)
            .await
            .expect("fallback");
        self.reload().await;
    }

    async fn post(&self, route: &str, key: &str, body: &Value) -> (u16, String) {
        let resp = reqwest::Client::new()
            .post(format!("{}{route}", self.base))
            .bearer_auth(key)
            .json(body)
            .send()
            .await
            .expect("send");
        let status = resp.status().as_u16();
        (status, resp.text().await.expect("body"))
    }

    async fn chat(&self, key: &str, model: &str) -> (u16, String) {
        self.post(
            "/v1/chat/completions",
            key,
            &json!({ "model": model, "messages": [{ "role": "user", "content": "hello there" }] }),
        )
        .await
    }

    async fn admin(&self, method: reqwest::Method, route: &str, account: &str) -> (u16, String) {
        let resp = reqwest::Client::new()
            .request(method, format!("{}{route}", self.base))
            .bearer_auth(ADMIN)
            .header("X-Lumen-Account-Ref", account)
            .send()
            .await
            .expect("send");
        let status = resp.status().as_u16();
        (status, resp.text().await.expect("body"))
    }

    /// The `GET /byok` view of one key of group A's pool for `provider`.
    async fn key_view(&self, provider: &str, key_id: &str) -> Value {
        let (status, body) = self
            .admin(
                reqwest::Method::GET,
                &format!("/admin/groups/{}/byok", self.group_a),
                ACCOUNT_A,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let listing: Value = serde_json::from_str(&body).expect("json");
        listing["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|p| p["name"] == provider)
            .and_then(|p| p["keys"].as_array())
            .and_then(|keys| keys.iter().find(|k| k["id"] == key_id))
            .cloned()
            .unwrap_or_else(|| panic!("key {key_id} missing in {listing}"))
    }

    /// Flush once and return the deltas of the key with `external_ref`.
    fn take(&self, external_ref: &str) -> Vec<UsageDelta> {
        let batch = self.runtime.keys.drain_flush(1_000);
        let mine = batch
            .deltas()
            .into_iter()
            .filter(|d| d.external_ref.as_deref() == Some(external_ref))
            .cloned()
            .collect();
        self.runtime.keys.commit_flush(batch);
        mine
    }

    /// Accumulate the deltas of `external_ref` until one settled request is
    /// seen (the settle can land after the client has the response).
    async fn settled(&self, external_ref: &str) -> UsageDelta {
        let mut total: Option<UsageDelta> = None;
        for _ in 0..200 {
            for delta in self.take(external_ref) {
                total = Some(match total {
                    None => delta,
                    Some(mut acc) => {
                        acc.cost_micro += delta.cost_micro;
                        acc.units.requests += delta.units.requests;
                        acc.units.tokens_in += delta.units.tokens_in;
                        acc.units.tokens_out += delta.units.tokens_out;
                        acc.units.byok_tokens_in += delta.units.byok_tokens_in;
                        acc.units.byok_tokens_out += delta.units.byok_tokens_out;
                        acc.units.bytes_in += delta.units.bytes_in;
                        acc.units.bytes_out += delta.units.bytes_out;
                        acc
                    }
                });
            }
            if let Some(delta) = &total {
                if delta.units.requests > 0 && delta.units.bytes_out > 0 {
                    return delta.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no settled request for {external_ref}: {total:?}");
    }

    /// Bytes credited to `external_ref` over a settle period.
    async fn quiet_bytes(&self, external_ref: &str) -> (i64, i64) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.take(external_ref).iter().fold((0, 0), |acc, d| {
            (acc.0 + d.units.bytes_in, acc.1 + d.units.bytes_out)
        })
    }
}

fn chat_body() -> Value {
    json!({
        "object": "chat.completion", "id": "c", "created": 1, "model": "gpt",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })
}

async fn mount(server: &MockServer, route: &str, bearer: &str, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(route))
        .and(header("authorization", format!("Bearer {bearer}").as_str()))
        .respond_with(template)
        .mount(server)
        .await;
}

async fn mount_ok(server: &MockServer, bearer: &str) {
    mount(
        server,
        "/chat/completions",
        bearer,
        ResponseTemplate::new(200).set_body_json(chat_body()),
    )
    .await;
}

async fn mount_status(server: &MockServer, bearer: &str, status: u16) {
    mount(
        server,
        "/chat/completions",
        bearer,
        ResponseTemplate::new(status).set_body_json(json!({ "error": { "message": "nope" } })),
    )
    .await;
}

/// Requests `server` received with `Bearer {bearer}`.
async fn hits(server: &MockServer, bearer: &str) -> usize {
    let want = format!("Bearer {bearer}");
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(want.as_str())
        })
        .count()
}

async fn servers() -> (MockServer, MockServer) {
    (MockServer::start().await, MockServer::start().await)
}

#[tokio::test]
async fn team_key_is_used_and_not_billed_tokens() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0);

    let delta = h.settled("lab-a").await;
    assert_eq!(delta.cost_micro, 0, "team-key tokens are not billed");
    assert_eq!(delta.units.tokens_in, 0);
    assert_eq!(delta.units.tokens_out, 0);
    assert_eq!(delta.units.byok_tokens_in, 5);
    assert_eq!(delta.units.byok_tokens_out, 3);
    assert!(delta.units.bytes_in > 0 && delta.units.bytes_out > 0);
}

#[tokio::test]
async fn team_key_serves_embeddings() {
    let (a, b) = servers().await;
    let embed = ResponseTemplate::new(200).set_body_json(json!({
        "object": "list",
        "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
        "model": "text-embedding-3-small",
        "usage": { "prompt_tokens": 4, "total_tokens": 4 }
    }));
    mount(&a, "/embeddings", TEAM_1, embed.clone()).await;
    mount(&a, "/embeddings", PLATFORM, embed).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    let (status, body) = h
        .post(
            "/v1/embeddings",
            &h.key_a,
            &json!({ "model": "model-a", "input": "abcd" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0);
    let delta = h.settled("lab-a").await;
    assert_eq!(delta.cost_micro, 0);
    assert_eq!(delta.units.tokens_in, 0);
    assert_eq!(delta.units.byok_tokens_in, 4);
}

#[tokio::test]
async fn no_pool_uses_platform_key_and_bills_tokens() {
    let (a, b) = servers().await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;

    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, PLATFORM).await, 1);

    let delta = h.settled("lab-a").await;
    assert!(
        delta.cost_micro > 0,
        "platform tokens are billed: {delta:?}"
    );
    assert_eq!(delta.units.tokens_in, 5);
    assert_eq!(delta.units.tokens_out, 3);
    assert_eq!(delta.units.byok_tokens_in, 0);
}

#[tokio::test]
async fn team_key_never_used_for_another_group() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    // Another team, and a key with no group at all: platform key only.
    for key in [&h.key_b, &h.key_solo] {
        let (status, body) = h.chat(key, "model-a").await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(hits(&a, TEAM_1).await, 0, "a team key served another group");
    assert_eq!(hits(&a, PLATFORM).await, 2);
    let delta = h.settled("lab-b").await;
    assert!(delta.cost_micro > 0);
    assert_eq!(delta.units.byok_tokens_in, 0);

    // The owning group does use it.
    let (status, _) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200);
    assert_eq!(hits(&a, TEAM_1).await, 1);

    // Group B's own pool for another provider does not unlock A's.
    h.add_key(&h.group_b, "b", TEAM_2).await;
    let (status, _) = h.chat(&h.key_b, "model-a").await;
    assert_eq!(status, 200);
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, TEAM_2).await, 0);
}

#[tokio::test]
async fn rate_limited_team_key_rotates_to_next() {
    let (a, b) = servers().await;
    mount(
        &a,
        "/chat/completions",
        TEAM_1,
        ResponseTemplate::new(429).insert_header("retry-after", "120"),
    )
    .await;
    mount_ok(&a, TEAM_2).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    let id_1 = h.add_key(&h.group_a, "a", TEAM_1).await;
    let id_2 = h.add_key(&h.group_a, "a", TEAM_2).await;

    // Two requests: whichever key the cursor starts on, round-robin makes the
    // rate-limited key come up once.
    for _ in 0..2 {
        let (status, body) = h.chat(&h.key_a, "model-a").await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(hits(&a, TEAM_1).await, 1, "a benched key is not retried");
    assert_eq!(hits(&a, TEAM_2).await, 2);
    assert_eq!(hits(&a, PLATFORM).await, 0);

    let view = h.key_view("a", &id_1).await;
    assert_eq!(view["status"], "rate_limited", "{view}");
    assert_eq!(h.key_view("a", &id_2).await["status"], "ok");
}

#[tokio::test]
async fn revoked_team_key_rotates_and_is_marked_failing() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 401).await;
    mount_ok(&a, TEAM_2).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    let id_1 = h.add_key(&h.group_a, "a", TEAM_1).await;
    let id_2 = h.add_key(&h.group_a, "a", TEAM_2).await;

    for _ in 0..2 {
        let (status, body) = h.chat(&h.key_a, "model-a").await;
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains(TEAM_1) && !body.contains(TEAM_2));
    }
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, TEAM_2).await, 2);
    assert_eq!(hits(&a, PLATFORM).await, 0);

    let view = h.key_view("a", &id_1).await;
    assert_eq!(view["status"], "failing", "{view}");
    assert!(view["status_detail"].is_string(), "{view}");
    assert!(!view.to_string().contains(TEAM_1));
    assert_eq!(h.key_view("a", &id_2).await["status"], "ok");
}

#[tokio::test]
async fn exhausted_pool_without_fallback_fails_lm_3021() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 429).await;
    mount_status(&a, TEAM_2, 429).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;
    h.add_key(&h.group_a, "a", TEAM_2).await;

    // The first request tries both keys upstream, then refuses.
    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 429, "{body}");
    let error: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(error["error"]["code"], "LM-3021", "{body}");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("'a'")),
        "{body}"
    );
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, TEAM_2).await, 1);
    // Bandwidth of that request (it did reach the upstream) is flushed away.
    let _ = h.quiet_bytes("lab-a").await;

    // Later requests while the keys are benched never reach the upstream,
    // so they bill no bandwidth.
    for _ in 0..6 {
        let (status, body) = h.chat(&h.key_a, "model-a").await;
        assert_eq!(status, 429, "{body}");
        assert!(body.contains("LM-3021"), "{body}");
    }
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, TEAM_2).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0, "the platform key was used");
    assert_eq!(h.quiet_bytes("lab-a").await, (0, 0));
    // Not a provider fault: the breaker never opened.
    assert_eq!(
        h.resilience.breakers.get("a", "model-a").state(),
        CircuitState::Closed
    );
}

#[tokio::test]
async fn exhausted_pool_without_fallback_never_reaches_platform_via_chain() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 429).await;
    mount_ok(&a, PLATFORM).await;
    mount_ok(&b, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    for _ in 0..2 {
        let (status, body) = h.chat(&h.key_a, "acme/chat").await;
        assert_eq!(status, 429, "{body}");
        assert!(body.contains("LM-3021"), "{body}");
    }
    assert_eq!(hits(&a, PLATFORM).await, 0);
    assert_eq!(
        hits(&b, PLATFORM).await,
        0,
        "the chain reached b's platform key"
    );

    // A team that serves b with its own keys does move on to b.
    h.add_key(&h.group_a, "b", TEAM_2).await;
    mount_ok(&b, TEAM_2).await;
    let (status, body) = h.chat(&h.key_a, "acme/chat").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&b, TEAM_2).await, 1);
    assert_eq!(hits(&b, PLATFORM).await, 0);
}

#[tokio::test]
async fn exhausted_pool_with_fallback_uses_platform_and_bills_tokens() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 429).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;
    h.set_fallback(&h.group_a, "a", true).await;

    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 1);

    let delta = h.settled("lab-a").await;
    assert!(
        delta.cost_micro > 0,
        "fallback traffic is billed: {delta:?}"
    );
    assert_eq!(delta.units.tokens_in, 5);
    assert_eq!(delta.units.byok_tokens_in, 0);
}

#[tokio::test]
async fn zero_credit_byok_request_is_refused() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_poor, "a", TEAM_1).await;

    let (status, body) = h.chat(&h.key_poor, "model-a").await;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("LM-4001"), "{body}");
    assert_eq!(a.received_requests().await.unwrap_or_default().len(), 0);
}

#[tokio::test]
async fn deleted_team_key_is_not_used_after_delete() {
    let (a, b) = servers().await;
    mount(
        &a,
        "/chat/completions",
        TEAM_1,
        ResponseTemplate::new(200)
            .set_delay(Duration::from_millis(600))
            .set_body_json(chat_body()),
    )
    .await;
    mount_ok(&a, PLATFORM).await;
    let h = Arc::new(harness(&a, &b).await);
    let id = h.add_key(&h.group_a, "a", TEAM_1).await;

    // A request in flight on the key when it is deleted still finishes.
    let in_flight = {
        let h = Arc::clone(&h);
        tokio::spawn(async move { h.chat(&h.key_a, "model-a").await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, body) = h
        .admin(
            reqwest::Method::DELETE,
            &format!("/admin/groups/{}/byok/a/keys/{id}", h.group_a),
            ACCOUNT_A,
        )
        .await;
    assert_eq!(status, 204, "{body}");
    h.reload().await;
    let (status, body) = in_flight.await.expect("join");
    assert_eq!(status, 200, "{body}");

    // New requests never use it.
    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 1);
}

#[tokio::test]
async fn pool_for_a_provider_whose_kind_became_ineligible_is_ignored() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    // A whole-document config write turns `a` into a self-hosted kind and
    // leaves the pool rows behind.
    let text = config_text(&a.uri(), &b.uri(), "vllm");
    std::fs::write(&h.config_path, &text).expect("write config");
    let config = Config::load_text(&text, "test").expect("valid config");
    h.registry.reload(specs(&config)).expect("registry reload");

    // Before and after the pools are rebuilt from the store.
    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    h.reload().await;
    let (status, body) = h.chat(&h.key_a, "model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        hits(&a, TEAM_1).await,
        0,
        "an ineligible kind used a team key"
    );
    assert_eq!(hits(&a, PLATFORM).await, 2);
    let delta = h.settled("lab-a").await;
    assert!(delta.cost_micro > 0);
    assert_eq!(delta.units.byok_tokens_in, 0);
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
async fn team_key_secret_never_logged() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufMakeWriter(buffer.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 401).await;
    mount(
        &a,
        "/chat/completions",
        TEAM_2,
        ResponseTemplate::new(429).insert_header("retry-after", "60"),
    )
    .await;
    let h = harness(&a, &b).await;
    let id_1 = h.add_key(&h.group_a, "a", TEAM_1).await;
    h.add_key(&h.group_a, "a", TEAM_2).await;

    // Rotation through a revoked and a rate-limited key, then LM-3021.
    let mut bodies = Vec::new();
    for _ in 0..2 {
        let (status, body) = h.chat(&h.key_a, "model-a").await;
        assert_eq!(status, 429, "{body}");
        bodies.push(body);
    }
    bodies.push(h.key_view("a", &id_1).await.to_string());

    let logs = String::from_utf8_lossy(&buffer.lock().expect("log buffer")).into_owned();
    assert!(!logs.is_empty(), "the subscriber captured nothing");
    for secret in [TEAM_1, TEAM_2, "-ONE-secret", "-TWO-secret"] {
        assert!(!logs.contains(secret), "a team key reached the logs");
        for body in &bodies {
            assert!(!body.contains(secret), "a team key reached a response");
        }
    }
}

/// An upstream that streams SSE frames forever and records each request's
/// `Authorization` header.
async fn endless_sse_upstream(seen: Arc<Mutex<Vec<String>>>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut req = [0u8; 8192];
                let n = socket.read(&mut req).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&req[..n]).to_lowercase();
                if let Some(auth) = text
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: bearer "))
                {
                    seen.lock().expect("seen").push(auth.trim().to_owned());
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for i in 0.. {
                    let frame = format!(
                        "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
                    );
                    if socket.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = socket.flush().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_on_team_key_cancel_settles_once() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = endless_sse_upstream(Arc::clone(&seen)).await;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    let h = harness(&a, &b).await;
    // Point provider `a` at the endless upstream.
    let text = config_text(&upstream, &b.uri(), "openai");
    let config = Config::load_text(&text, "test").expect("valid config");
    h.registry.reload(specs(&config)).expect("registry reload");
    h.add_key(&h.group_a, "a", TEAM_1).await;

    let mut resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&h.key_a)
        .json(&json!({
            "model": "model-a",
            "messages": [{ "role": "user", "content": "hello there" }],
            "stream": true
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let first = resp.chunk().await.expect("chunk").expect("a first frame");
    assert!(!first.is_empty());
    drop(resp);

    let delta = h.settled("lab-a").await;
    assert_eq!(delta.units.requests, 1, "settled exactly once");
    assert_eq!(delta.cost_micro, 0);
    assert_eq!(delta.units.tokens_in, 0);
    assert!(delta.units.byok_tokens_in > 0, "{delta:?}");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        [TEAM_1.to_lowercase()],
        "the stream was opened on the team key"
    );
    // Nothing more is settled afterwards.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.take("lab-a").iter().all(|d| d.units.requests == 0),
        "the stream settled twice"
    );
}
