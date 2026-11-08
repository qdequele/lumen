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
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
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
/// traffic has a visible cost, and fallback virtual models a -> b and b -> a.
fn config_text(a: &str, b: &str, kind_a: &str) -> String {
    config_text_with(a, b, kind_a, "")
}

/// [`config_text`] with extra keys on provider `a` (e.g. a timeout).
fn config_text_with(a: &str, b: &str, kind_a: &str, extra_a: &str) -> String {
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
        {extra_a}
        [[providers.models]]
        id = "a/model-a"
        capabilities = ["chat", "embed"]
        cost_per_1m_input = 1000.0
        cost_per_1m_output = 2000.0

        [[providers]]
        name = "b"
        kind = "openai"
        base_url = "{b}"
        [[providers.models]]
        id = "b/model-b"
        capabilities = ["chat"]
        cost_per_1m_input = 1000.0
        cost_per_1m_output = 2000.0

        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "a/model-a" }}, {{ model = "b/model-b" }}]

        [[virtual_models]]
        id = "acme/reverse"
        capability = "chat"
        strategy = "fallback"
        targets = [{{ model = "b/model-b" }}, {{ model = "a/model-a" }}]
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
    harness_with(a, b, "").await
}

#[allow(clippy::too_many_lines)] // one linear fixture: stores, state, server
async fn harness_with(a: &MockServer, b: &MockServer, extra_a: &str) -> Harness {
    let text = config_text_with(&a.uri(), &b.uri(), "openai", extra_a);
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
                parent_id: None,
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
    let (logger, _writer) = spawn_usage_writer(
        runtime.store.clone(),
        UsageWriterConfig {
            capacity: 64,
            batch_max: 500,
            flush_interval: Duration::from_millis(25),
        },
    );
    let state = AppState::new(metrics, Arc::clone(&registry), tokens, latency)
        .with_usage(logger)
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

    /// [`Self::chat`], returning the `x-lumen-cost-micro-usd` header too.
    async fn chat_cost(&self, key: &str, model: &str) -> (u16, Option<String>) {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(key)
            .json(&json!({ "model": model, "messages": [{ "role": "user", "content": "hello there" }] }))
            .send()
            .await
            .expect("send");
        let cost = resp
            .headers()
            .get("x-lumen-cost-micro-usd")
            .map(|v| v.to_str().expect("ascii").to_owned());
        (resp.status().as_u16(), cost)
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

    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
            &json!({ "model": "a/model-a", "input": "abcd" }),
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

    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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

/// The cost header reports what the call settled: the platform cost, which
/// is what the group is billed for it (5 in x $1000/1M + 3 out x $2000/1M).
#[tokio::test]
async fn platform_call_reports_its_settled_cost_in_a_header() {
    let (a, b) = servers().await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;

    let (status, cost) = h.chat_cost(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200);
    assert_eq!(cost.as_deref(), Some("11000"));
    let delta = h.settled("lab-a").await;
    assert_eq!(
        delta.cost_micro, 11000,
        "the header matches the settled cost"
    );
}

/// A team key paid the provider: nothing is billed, the header says 0.
#[tokio::test]
async fn team_key_call_reports_a_zero_cost_header() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    let (status, cost) = h.chat_cost(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200);
    assert_eq!(cost.as_deref(), Some("0"));
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
        let (status, body) = h.chat(key, "a/model-a").await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(hits(&a, TEAM_1).await, 0, "a team key served another group");
    assert_eq!(hits(&a, PLATFORM).await, 2);
    let delta = h.settled("lab-b").await;
    assert!(delta.cost_micro > 0);
    assert_eq!(delta.units.byok_tokens_in, 0);

    // The owning group does use it.
    let (status, _) = h.chat(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200);
    assert_eq!(hits(&a, TEAM_1).await, 1);

    // Group B's own pool for another provider does not unlock A's.
    h.add_key(&h.group_b, "b", TEAM_2).await;
    let (status, _) = h.chat(&h.key_b, "a/model-a").await;
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
        let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
        let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
        let (status, body) = h.chat(&h.key_a, "a/model-a").await;
        assert_eq!(status, 429, "{body}");
        assert!(body.contains("LM-3021"), "{body}");
    }
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, TEAM_2).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0, "the platform key was used");
    assert_eq!(h.quiet_bytes("lab-a").await, (0, 0));
    // Not a provider fault: the breaker never opened.
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
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

    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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

    let (status, body) = h.chat(&h.key_poor, "a/model-a").await;
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
        tokio::spawn(async move { h.chat(&h.key_a, "a/model-a").await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, body) = h
        .admin(
            reqwest::Method::DELETE,
            &format!("/admin/groups/{}/byok/a/keys/{id}", h.group_a),
            ACCOUNT_A,
        )
        .await;
    // No manual reload: the delete route itself evicts the key.
    assert_eq!(status, 204, "{body}");
    let (status, body) = in_flight.await.expect("join");
    assert_eq!(status, 200, "{body}");

    // New requests never use it.
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    h.reload().await;
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
    // Another test may have cached "never" for these callsites first.
    tracing::callsite::rebuild_interest_cache();

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
        let (status, body) = h.chat(&h.key_a, "a/model-a").await;
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
            "model": "a/model-a",
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

/// Chain a -> b, a team pool for `a` with the fallback off, no pool for `b`:
/// when link `a` never ran upstream (its circuit is open), link `b` refuses
/// (LM-3021) and b's platform key is never used.
async fn assert_chain_refused(h: &Harness, b: &MockServer) {
    let (status, body) = h.chat(&h.key_a, "acme/chat").await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("LM-3021"), "{body}");
    assert_eq!(
        hits(b, PLATFORM).await,
        0,
        "the chain reached b's platform key"
    );
}

/// Chain a -> b as in [`assert_chain_refused`], where link `a` failed
/// upstream: link `b` still refuses, but the client sees a's own error
/// (`code`), not the refusal that would hide it.
async fn assert_chain_surfaces(h: &Harness, b: &MockServer, code: &str) {
    let (status, body) = h.chat(&h.key_a, "acme/chat").await;
    let error: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(error["error"]["code"], code, "{status} {body}");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("'a'")),
        "{body}"
    );
    assert_eq!(
        hits(b, PLATFORM).await,
        0,
        "the chain reached b's platform key"
    );
}

#[tokio::test]
async fn chain_after_team_key_5xx_never_reaches_platform() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 500).await;
    mount_ok(&a, PLATFORM).await;
    mount_ok(&b, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    assert_chain_surfaces(&h, &b, "LM-3003").await;
    assert_eq!(hits(&a, TEAM_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0);
}

#[tokio::test]
async fn chain_after_team_key_first_token_timeout_never_reaches_platform() {
    let (a, b) = servers().await;
    mount(
        &a,
        "/chat/completions",
        TEAM_1,
        ResponseTemplate::new(200)
            .set_delay(Duration::from_secs(3))
            .set_body_json(chat_body()),
    )
    .await;
    mount_ok(&a, PLATFORM).await;
    mount_ok(&b, PLATFORM).await;
    let h = harness_with(&a, &b, "first_token_timeout_ms = 200").await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    assert_chain_surfaces(&h, &b, "LM-3011").await;
    assert_eq!(hits(&a, PLATFORM).await, 0);
}

#[tokio::test]
async fn chain_after_open_circuit_on_team_link_never_reaches_platform() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    mount_ok(&b, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;
    // Link a is served from group A's pool: its own breaker scope.
    let breaker = h
        .resilience
        .breakers
        .get_scoped("a", Some(&h.group_a), "a/model-a");
    for _ in 0..20 {
        breaker.on_failure(tokio::time::Instant::now());
    }
    assert_eq!(breaker.state(), CircuitState::Open);

    assert_chain_refused(&h, &b).await;
    assert_eq!(hits(&a, TEAM_1).await, 0, "the open circuit skipped link a");
    assert_eq!(hits(&a, PLATFORM).await, 0);
}

#[tokio::test]
async fn platform_link_before_the_team_link_still_serves() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&b, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    // Chain b -> a: b comes before the team link, so it keeps its platform key.
    let (status, body) = h.chat(&h.key_a, "acme/reverse").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&b, PLATFORM).await, 1);
    assert_eq!(hits(&a, TEAM_1).await, 0);
    let delta = h.settled("lab-a").await;
    assert!(
        delta.cost_micro > 0,
        "platform traffic is billed: {delta:?}"
    );
}

#[tokio::test]
async fn team_key_usage_is_recorded_at_zero_cost() {
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    for key in [&h.key_a, &h.key_b] {
        let (status, body) = h.chat(key, "a/model-a").await;
        assert_eq!(status, 200, "{body}");
    }
    let group = |listing: &Value, id: &str| {
        listing["groups"]
            .as_array()
            .and_then(|g| g.iter().find(|row| row["group"] == id))
            .cloned()
    };
    let mut listing = Value::Null;
    for _ in 0..100 {
        let resp = reqwest::Client::new()
            .get(format!("{}/admin/usage?group_by=group_id", h.base))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
        listing = resp.json().await.expect("json");
        if group(&listing, &h.group_a).is_some() && group(&listing, &h.group_b).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let team = group(&listing, &h.group_a).unwrap_or_else(|| panic!("no team row: {listing}"));
    assert_eq!(team["cost"].as_f64(), Some(0.0), "{team}");
    assert_eq!(team["tokens_in"], 5, "{team}");
    assert_eq!(team["tokens_out"], 3, "{team}");
    let platform = group(&listing, &h.group_b).unwrap_or_else(|| panic!("no row: {listing}"));
    assert!(
        platform["cost"].as_f64().is_some_and(|c| c > 0.0),
        "{platform}"
    );
}

#[tokio::test]
async fn platform_rate_limits_do_not_block_a_team_with_its_own_keys() {
    let (a, b) = servers().await;
    mount_status(&a, PLATFORM, 429).await;
    mount_ok(&a, TEAM_1).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    // Five platform 429s (another team, no pool) open a's platform breaker.
    for _ in 0..5 {
        let (status, body) = h.chat(&h.key_b, "a/model-a").await;
        assert_eq!(status, 429, "{body}");
    }
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
        CircuitState::Open
    );
    let (_, body) = h.chat(&h.key_b, "a/model-a").await;
    assert!(body.contains("LM-3020"), "{body}");

    // Group A, with its own key for a, is still served on it.
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1);
    // And its success leaves the platform breaker open.
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
        CircuitState::Open
    );
}

#[tokio::test]
async fn team_key_failures_never_open_the_platform_breaker() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 500).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    for _ in 0..5 {
        let (status, body) = h.chat(&h.key_a, "a/model-a").await;
        assert_eq!(status, 502, "{body}");
    }
    assert_eq!(
        h.resilience
            .breakers
            .get_scoped("a", Some(&h.group_a), "a/model-a")
            .state(),
        CircuitState::Open
    );
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
        CircuitState::Closed
    );
    // A request without a pool is served on the platform key.
    let (status, body) = h.chat(&h.key_b, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, PLATFORM).await, 1);
}

// ---- Platform key pools (ADR 019) -------------------------------------------

const POOL_1: &str = "sk-platform-POOL-one-3333";
const POOL_2: &str = "sk-platform-POOL-two-4444";

impl Harness {
    /// Seal a platform key for `provider` and reload. Returns the key id.
    async fn add_platform_key(&self, provider: &str, secret: &str) -> String {
        let record = self
            .runtime
            .store
            .add_platform_provider_key(provider, "platform", secret, &master())
            .await
            .expect("add platform key");
        self.reload().await;
        record.id
    }
}

#[tokio::test]
async fn platform_pool_rotates_instead_of_the_configured_key_and_bills_tokens() {
    let (a, b) = servers().await;
    mount_status(&a, POOL_1, 429).await;
    mount_ok(&a, POOL_2).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    let first = h.add_platform_key("a", POOL_1).await;
    h.add_platform_key("a", POOL_2).await;

    // No group, and a group without a team pool: both use the platform pool.
    let (status, body) = h.chat(&h.key_solo, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = h.chat(&h.key_b, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    let delta = h.settled("lab-b").await;
    assert!(
        delta.cost_micro > 0,
        "platform pool tokens are billed: {delta:?}"
    );
    assert_eq!(delta.units.tokens_in, 5);
    assert_eq!(delta.units.byok_tokens_in, 0);
    assert_eq!(hits(&a, POOL_1).await, 1, "benched after its 429");
    assert_eq!(hits(&a, POOL_2).await, 2);
    assert_eq!(
        hits(&a, PLATFORM).await,
        0,
        "the configured key is ignored once a pool exists"
    );
    assert_eq!(h.pools.platform_status(&first).0, "rate_limited");
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
        CircuitState::Closed,
        "a rotated 429 is not a provider fault"
    );
}

#[tokio::test]
async fn exhausted_platform_pool_never_uses_the_configured_key() {
    let (a, b) = servers().await;
    mount_status(&a, POOL_1, 401).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    let id = h.add_platform_key("a", POOL_1).await;

    let (status, body) = h.chat(&h.key_solo, "a/model-a").await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(hits(&a, POOL_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0, "the env key is ignored");
    assert_eq!(h.pools.platform_status(&id).0, "failing");
}

#[tokio::test]
async fn a_pool_of_disabled_keys_ignores_the_env_key_until_its_last_key_is_deleted() {
    let (a, b) = servers().await;
    mount_ok(&a, POOL_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    let id = h.add_platform_key("a", POOL_1).await;
    h.runtime
        .store
        .set_platform_provider_key_enabled("a", &id, false)
        .await
        .expect("disable")
        .expect("found");
    h.reload().await;

    let (status, body) = h.chat(&h.key_solo, "a/model-a").await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("LM-3022"), "{body}");
    assert_eq!(hits(&a, POOL_1).await, 0);
    assert_eq!(hits(&a, PLATFORM).await, 0, "the env key is ignored");
    assert_eq!(
        h.resilience.breakers.get("a", "a/model-a").state(),
        CircuitState::Closed,
        "a disabled pool is not a provider fault"
    );

    // No platform key left: the env key serves again.
    assert!(h
        .runtime
        .store
        .delete_platform_provider_key("a", &id)
        .await
        .expect("delete"));
    h.reload().await;
    let (status, body) = h.chat(&h.key_solo, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, PLATFORM).await, 1);
}

#[tokio::test]
async fn fallback_off_team_never_reaches_the_platform_pool_even_via_chain() {
    let (a, b) = servers().await;
    mount_status(&a, TEAM_1, 429).await;
    for server in [&a, &b] {
        mount_ok(server, POOL_1).await;
        mount_ok(server, PLATFORM).await;
    }
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;
    h.add_platform_key("a", POOL_1).await;
    h.add_platform_key("b", POOL_1).await;

    for model in ["a/model-a", "acme/chat"] {
        let (status, body) = h.chat(&h.key_a, model).await;
        assert_eq!(status, 429, "{model}: {body}");
        assert!(body.contains("LM-3021"), "{body}");
    }
    for server in [&a, &b] {
        assert_eq!(
            hits(server, POOL_1).await,
            0,
            "a platform pool key was used"
        );
        assert_eq!(hits(server, PLATFORM).await, 0);
    }

    // Fallback on: the platform pool serves after the team keys.
    h.set_fallback(&h.group_a, "a", true).await;
    let (status, body) = h.chat(&h.key_a, "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, POOL_1).await, 1);
    assert_eq!(hits(&a, PLATFORM).await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn platform_pool_secrets_never_logged() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufMakeWriter(buffer.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // Another test may have cached "never" for these callsites first.
    tracing::callsite::rebuild_interest_cache();

    let (a, b) = servers().await;
    mount_status(&a, POOL_1, 429).await;
    mount_status(&a, POOL_2, 401).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_platform_key("a", POOL_1).await;
    h.add_platform_key("a", POOL_2).await;

    // Both keys rotated out: the pool's last error, never the env key. The
    // rotation's starting key varies, so the last error is the 401 (502) or
    // the 429.
    let (status, body) = h.chat(&h.key_solo, "a/model-a").await;
    assert!(matches!(status, 429 | 502), "status {status}: {body}");
    let logs = String::from_utf8_lossy(&buffer.lock().expect("log buffer")).into_owned();
    assert!(!logs.is_empty(), "the subscriber captured nothing");
    for secret in [POOL_1, POOL_2, "-POOL-one", "-POOL-two"] {
        assert!(!logs.contains(secret), "a platform key reached the logs");
        assert!(!body.contains(secret), "a platform key reached a response");
    }
}

#[tokio::test]
async fn a_sub_group_key_uses_its_parents_team_key_and_bills_its_account() {
    // ADR 020: team keys and billing resolve through the top-level group.
    let (a, b) = servers().await;
    mount_ok(&a, TEAM_1).await;
    mount_ok(&a, PLATFORM).await;
    let h = harness(&a, &b).await;
    h.add_key(&h.group_a, "a", TEAM_1).await;

    let store = &h.runtime.store;
    let sub = store
        .create_group(NewGroup {
            name: "staging".to_owned(),
            budget_max: Some(10.0),
            account_ref: None,
            parent_id: Some(h.group_a.clone()),
        })
        .await
        .expect("sub-group");
    h.runtime.keys.upsert_group(&sub);
    let (plain, record) = store
        .create_key(NewKey {
            name: "sub".to_owned(),
            group_id: Some(sub.id.clone()),
            external_ref: Some("lab-sub".to_owned()),
            ..NewKey::default()
        })
        .await
        .expect("key");
    h.runtime.keys.upsert(hash_key(plain.reveal()), &record);

    let (status, body) = h.chat(plain.reveal(), "a/model-a").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, TEAM_1).await, 1, "the parent's team key served it");
    assert_eq!(hits(&a, PLATFORM).await, 0);
    let delta = h.settled("lab-sub").await;
    assert_eq!(delta.account_ref, ACCOUNT_A);
    assert_eq!(delta.cost_micro, 0);
    assert_eq!(delta.units.byok_tokens_in, 5);
}

/// ADR 021: a cold-start 503 retried on the provider's own policy (the
/// global one is a single attempt here) bills exactly one request with the
/// usage of the attempt that succeeded, never one per attempt.
#[tokio::test]
async fn cold_start_retry_on_the_provider_policy_bills_once() {
    let (a, b) = servers().await;
    // First-mounted wins until the 503 is used up.
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "Model BAAI/bge-m3 is currently loading",
            "estimated_time": 0.05
        })))
        .up_to_n_times(1)
        .mount(&a)
        .await;
    mount(
        &a,
        "/embeddings",
        PLATFORM,
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
            "model": "bge-m3",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        })),
    )
    .await;
    let h = harness_with(&a, &b, "retry_max_attempts = 3\nretry_base_ms = 10").await;

    let (status, body) = h
        .post(
            "/v1/embeddings",
            &h.key_a,
            &json!({ "model": "a/model-a", "input": "abcd" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hits(&a, PLATFORM).await, 2, "503 then 200");

    let delta = h.settled("lab-a").await;
    assert_eq!(delta.units.requests, 1, "one billed request: {delta:?}");
    assert_eq!(delta.units.tokens_in, 4, "the success's usage only");
    // Nothing else settles later for this request.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let late: i64 = h.take("lab-a").iter().map(|d| d.units.requests).sum();
    assert_eq!(late, 0, "no second settle");
}
