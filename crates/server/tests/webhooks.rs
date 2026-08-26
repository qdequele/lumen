//! End-to-end tests for outbound budget webhooks (ADR 011, issue #146).
//!
//! A full auth-enabled gateway sits in front of a wiremock upstream, with a
//! second wiremock server standing in for the billing backend's receiver.
//! Traffic flows through `/v1/chat/completions` and the assertions are made on
//! what the receiver actually got: the payload, the headers, the HMAC, the
//! retry behaviour and the Prometheus counters.

// Exact float literals stored and read back unchanged - strict equality is
// the correct assertion for these budget figures.
#![allow(clippy::float_cmp)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use hmac::{Hmac, KeyInit, Mac};
use lumen_auth::crypto::MasterKey;
use lumen_auth::events::{EventKind, WebhookSettings};
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::auth::AuthRuntime;
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::webhooks::WebhookController;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;
const SECRET: &str = "whsec-test-do-not-log-this";
/// A second secret, for the rotation assertions.
const ROTATED: &str = "whsec-rotated-value";

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

/// $1 per token in and out: the mocked upstream reports 12 in / 34 out, so
/// every successful call settles at exactly $46. The pre-call estimate for the
/// fixed request below is $10.
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

/// A `[webhooks]` block pointed at `receiver`, with fast retries so the tests
/// stay quick.
fn webhook_config(receiver: &str, events: Vec<EventKind>, thresholds: Vec<u8>) -> WebhookSettings {
    WebhookSettings {
        url: format!("{receiver}/events"),
        signing_key_env: None,
        events,
        thresholds,
        channel_capacity: 64,
        timeout_ms: 2_000,
        max_attempts: 3,
        retry_base_ms: 20,
    }
}

struct Harness {
    base: String,
    client: reqwest::Client,
    metrics: Metrics,
    cancel: CancellationToken,
    store: KeyStore,
    controller: Arc<WebhookController>,
    auth: Arc<AuthRuntime>,
}

impl Harness {
    async fn create_key(&self, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("create key")
    }

    async fn patch_key(&self, id: &str, body: &Value) -> reqwest::Response {
        self.client
            .patch(format!("{}/admin/keys/{id}", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("patch key")
    }

    async fn rotate_key(&self, id: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys/{id}/rotate", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("rotate key")
    }

    async fn delete_key(&self, id: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/admin/keys/{id}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("delete key")
    }

    async fn grant_key(&self, id: &str, amount: f64) -> reqwest::Response {
        self.client
            .post(format!("{}/admin/keys/{id}/grant", self.base))
            .bearer_auth(master())
            .json(&json!({ "amount": amount }))
            .send()
            .await
            .expect("grant key")
    }

    /// One fixed chat call: settles at $46 with the test pricing.
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

    fn metrics_text(&self) -> String {
        self.metrics.encode_text()
    }
}

/// Spawn an auth-enabled gateway with the webhook stack wired up exactly as
/// `main` does, and hand back the harness.
///
/// `signing_secret`, when given, is sealed into the store before the controller
/// reads it - the `PUT /admin/webhooks/signing-key` path - so these tests
/// exercise the same resolution a real boot does.
async fn spawn_gateway(
    registry: Arc<Registry>,
    webhooks: &WebhookSettings,
    signing_secret: Option<&str>,
) -> Harness {
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

    if let Some(secret) = signing_secret {
        store
            .store_webhook_secret(
                secret,
                &MasterKey::from_env_value(&master()).expect("master"),
            )
            .await
            .expect("seal the signing secret");
    }
    let cancel = CancellationToken::new();
    let controller = Arc::new(WebhookController::new(
        metrics.clone(),
        http::build_client(),
        cancel.clone(),
    ));
    controller
        .refresh_from_store(&store, runtime.master.as_ref())
        .await;
    controller
        .apply(webhooks, &runtime.keys)
        .expect("the test webhook settings apply");

    let state = AppState::new(metrics.clone(), registry, tokens, latency)
        .with_pricing(dollar_pricing())
        .with_auth(Arc::clone(&runtime))
        .with_usage(logger)
        .with_webhooks(Arc::clone(&controller));
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        client: reqwest::Client::new(),
        metrics,
        cancel,
        store,
        controller,
        auth: runtime,
    }
}

/// The same gateway assembly with an inert controller: auth on, no
/// `[webhooks]` block, nothing ever applied. This is what every other
/// integration test builds, and the shape a control plane provisions into.
async fn spawn_gateway_without_webhooks(registry: Arc<Registry>) -> Harness {
    let store = KeyStore::in_memory().await.expect("open store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let cancel = CancellationToken::new();
    let controller = Arc::new(WebhookController::new(
        metrics.clone(),
        http::build_client(),
        cancel.clone(),
    ));
    let state = AppState::new(metrics.clone(), registry, tokens, latency)
        .with_pricing(dollar_pricing())
        .with_auth(Arc::clone(&runtime))
        .with_webhooks(Arc::clone(&controller));
    let base = common::spawn_state(state, LIMIT).await;
    Harness {
        base,
        client: reqwest::Client::new(),
        metrics,
        cancel,
        store,
        controller,
        auth: runtime,
    }
}

/// Mount a receiver that always answers 200 and records what it got.
async fn mount_receiver(receiver: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200))
        .mount(receiver)
        .await;
}

/// The JSON bodies the receiver has been sent so far.
async fn received_events(receiver: &MockServer) -> Vec<Value> {
    receiver
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/events")
        .map(|request| serde_json::from_slice(&request.body).expect("webhook body is valid JSON"))
        .collect()
}

/// Every recorded delivery to `/events`, headers included.
async fn received_requests(receiver: &MockServer) -> Vec<Request> {
    receiver
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/events")
        .collect()
}

/// Poll until the receiver has recorded at least `count` deliveries, or the
/// deadline passes. Delivery is asynchronous by design, so the tests wait on
/// the observable outcome instead of sleeping a fixed amount.
async fn wait_for_events(receiver: &MockServer, count: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = received_events(receiver).await;
        if events.len() >= count || std::time::Instant::now() > deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Hex HMAC-SHA256, computed independently of the implementation under test.
fn expected_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn header(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

// ---- Threshold crossing -----------------------------------------------------

#[tokio::test]
async fn a_threshold_crossing_delivers_exactly_one_signed_event() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;

    // 50 % and 95 %: the two settles below land at 46 % and 92 %, so exactly
    // one crossing happens and the second threshold stays armed.
    let config = webhook_config(
        &receiver.uri(),
        vec![EventKind::BudgetThreshold],
        vec![50, 95],
    );
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, Some(SECRET)).await;

    // $100 budget; one call settles at $46 (under 50 %), the second at $92
    // (past 50 % but under 80 %).
    let created: Value = h
        .create_key(&json!({ "name": "prepaid", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    let key_id = created["id"].as_str().expect("key id").to_owned();

    assert_eq!(h.chat(&key).await.status(), 200);
    assert_eq!(h.chat(&key).await.status(), 200);

    let events = wait_for_events(&receiver, 1).await;
    // Delivery is asynchronous: let any second event that was going to arrive
    // arrive, so "exactly one" is a real assertion and not a race.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let events_after = received_events(&receiver).await;
    assert_eq!(
        events_after.len(),
        events.len(),
        "one crossing must deliver one event: {events_after:?}"
    );
    assert_eq!(
        events.len(),
        1,
        "exactly one crossing, one event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["event"], "budget.threshold");
    assert_eq!(event["scope"], "key");
    assert_eq!(event["subject_id"], key_id.as_str());
    assert_eq!(event["subject_name"], "prepaid");
    assert_eq!(event["threshold"], 50);
    assert_eq!(event["budget_max"], 100.0);
    assert_eq!(event["budget_spent"], 92.0);
    assert!(event["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("evt_")));

    // The signature is the HMAC of the exact bytes that were sent, and the
    // headers agree with the signed body.
    let requests = received_requests(&receiver).await;
    let request = &requests[0];
    assert_eq!(
        header(request, "x-lumen-signature"),
        expected_signature(SECRET, &request.body)
    );
    assert_eq!(header(request, "x-lumen-event-id"), event["id"]);
    assert_eq!(header(request, "x-lumen-event"), "budget.threshold");
    assert_eq!(
        header(request, "x-lumen-timestamp"),
        event["ts"].to_string()
    );
    assert_eq!(header(request, "content-type"), "application/json");

    // Further calls under the next threshold add nothing: edge-triggered.
    assert_eq!(h.chat(&key).await.status(), 402, "the $100 budget is spent");
    let events = received_events(&receiver).await;
    assert_eq!(events.len(), 1, "no re-fire per request: {events:?}");

    assert!(h.metrics_text().contains("lumen_webhook_sent_total 1"));
}

#[tokio::test]
async fn the_payload_never_carries_a_key_secret_or_any_content() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;

    let config = webhook_config(&receiver.uri(), EventKind::ALL.to_vec(), vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, Some(SECRET)).await;

    let created: Value = h
        .create_key(&json!({ "name": "prepaid", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();

    assert_eq!(h.chat(&key).await.status(), 200);
    let events = wait_for_events(&receiver, 1).await;
    assert!(!events.is_empty());

    for request in received_requests(&receiver).await {
        let body = String::from_utf8_lossy(&request.body).to_string();
        let headers = format!("{:?}", request.headers);
        // No plaintext key, no signing secret, no prompt or completion text.
        assert!(!body.contains(&key), "plaintext key leaked: {body}");
        assert!(!body.contains(SECRET), "signing secret leaked: {body}");
        assert!(
            !headers.contains(SECRET),
            "signing secret leaked: {headers}"
        );
        assert!(!body.contains("hi"), "prompt content leaked: {body}");
        assert!(!body.contains("hello"), "completion content leaked: {body}");
    }
}

// ---- Exhaustion -------------------------------------------------------------

#[tokio::test]
async fn the_first_refused_request_delivers_budget_exhausted_once() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;

    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetExhausted], vec![]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    // $20 budget: the first call's $10 estimate fits, but it settles at $46,
    // so every later call is refused with 402 (LM-4001).
    let created: Value = h
        .create_key(&json!({ "name": "small", "budget_max": 20.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    let key_id = created["id"].as_str().expect("key id").to_owned();

    assert_eq!(h.chat(&key).await.status(), 200);
    for _ in 0..3 {
        assert_eq!(h.chat(&key).await.status(), 402);
    }

    let events = wait_for_events(&receiver, 1).await;
    assert_eq!(events.len(), 1, "exhaustion is edge-triggered: {events:?}");
    assert_eq!(events[0]["event"], "budget.exhausted");
    assert_eq!(events[0]["scope"], "key");
    assert_eq!(events[0]["subject_id"], key_id.as_str());
    assert!(events[0].get("threshold").is_none());

    // An auto-recharge closes the loop: the grant re-arms the signal and the
    // key serves traffic again.
    assert_eq!(h.grant_key(&key_id, 100.0).await.status(), 200);
    assert_eq!(h.chat(&key).await.status(), 200);
    let events = received_events(&receiver).await;
    assert_eq!(
        events.len(),
        1,
        "a grant does not itself signal: {events:?}"
    );
}

// ---- Lifecycle --------------------------------------------------------------

#[tokio::test]
async fn key_lifecycle_changes_are_delivered_once_each() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;

    let config = webhook_config(
        &receiver.uri(),
        vec![
            EventKind::KeyDisabled,
            EventKind::KeyRotated,
            EventKind::KeyDeleted,
        ],
        vec![],
    );
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let created: Value = h
        .create_key(&json!({ "name": "lifecycle", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key_id = created["id"].as_str().expect("key id").to_owned();

    // Rotate, then disable (twice - the second is not a state change), then
    // delete.
    assert_eq!(h.rotate_key(&key_id).await.status(), 200);
    assert_eq!(
        h.patch_key(&key_id, &json!({ "disabled": true }))
            .await
            .status(),
        200
    );
    assert_eq!(
        h.patch_key(&key_id, &json!({ "disabled": true }))
            .await
            .status(),
        200
    );
    assert_eq!(h.delete_key(&key_id).await.status(), 204);

    let events = wait_for_events(&receiver, 3).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        ["key.rotated", "key.disabled", "key.deleted"],
        "one event per real state change: {events:?}"
    );
    for event in &events {
        assert_eq!(event["subject_id"], key_id.as_str());
        assert_eq!(event["subject_name"], "lifecycle");
    }
}

// ---- Delivery semantics -----------------------------------------------------

#[tokio::test]
async fn a_receiver_5xx_is_retried_with_backoff_until_it_succeeds() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    // Two 503s, then a 200: `max_attempts = 3` is exactly enough.
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&receiver)
        .await;
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let created: Value = h
        .create_key(&json!({ "name": "retry", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    // Three deliveries of the SAME event: same id, same signature-relevant
    // body, so a receiver can deduplicate.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let requests = loop {
        let requests = received_requests(&receiver).await;
        if requests.len() >= 3 || std::time::Instant::now() > deadline {
            break requests;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(requests.len(), 3, "two failures then a success");
    let ids: Vec<String> = requests
        .iter()
        .map(|r| header(r, "x-lumen-event-id"))
        .collect();
    assert_eq!(ids[0], ids[1], "a retry keeps the event id");
    assert_eq!(ids[1], ids[2], "a retry keeps the event id");
    assert!(!ids[0].is_empty());

    let metrics = h.metrics_text();
    assert!(metrics.contains("lumen_webhook_sent_total 1"), "{metrics}");
    assert!(
        metrics.contains("lumen_webhook_retries_total 2"),
        "{metrics}"
    );
    assert!(metrics.contains("lumen_webhook_dead_total 0"), "{metrics}");
}

#[tokio::test]
async fn an_event_the_receiver_never_accepts_is_abandoned_and_counted() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&receiver)
        .await;

    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let created: Value = h
        .create_key(&json!({ "name": "dead", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if h.metrics_text().contains("lumen_webhook_dead_total 1")
            || std::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let metrics = h.metrics_text();
    assert!(metrics.contains("lumen_webhook_dead_total 1"), "{metrics}");
    assert!(metrics.contains("lumen_webhook_sent_total 0"), "{metrics}");
    // `max_attempts = 3`, so two retries were counted before giving up.
    assert!(
        metrics.contains("lumen_webhook_retries_total 2"),
        "{metrics}"
    );
    assert_eq!(received_requests(&receiver).await.len(), 3);
}

#[tokio::test]
async fn a_permanent_rejection_is_not_retried() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&receiver)
        .await;

    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let created: Value = h
        .create_key(&json!({ "name": "rejected", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if h.metrics_text().contains("lumen_webhook_dead_total 1")
            || std::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let metrics = h.metrics_text();
    assert!(metrics.contains("lumen_webhook_dead_total 1"), "{metrics}");
    assert!(
        metrics.contains("lumen_webhook_retries_total 0"),
        "{metrics}"
    );
    assert_eq!(
        received_requests(&receiver).await.len(),
        1,
        "a 400 means this event will never be accepted"
    );
}

#[tokio::test]
async fn a_full_queue_drops_events_without_slowing_a_request_down() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    // The receiver never answers within the timeout, so the sender is stuck on
    // its first event and the queue behind it fills.
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&receiver)
        .await;

    let config = WebhookSettings {
        // Capacity 1: the sender holds one event, the queue holds one more,
        // and everything after that is dropped.
        channel_capacity: 1,
        max_attempts: 1,
        ..webhook_config(
            &receiver.uri(),
            vec![EventKind::BudgetThreshold],
            (1..=100).collect(),
        )
    };
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    // One threshold per percent: a single $46 settle against a $100 budget
    // crosses 46 of them at once, far more than the queue can hold.
    let created: Value = h
        .create_key(&json!({ "name": "flood", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();

    let started = std::time::Instant::now();
    assert_eq!(h.chat(&key).await.status(), 200);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the request must not wait on the webhook receiver (took {elapsed:?})"
    );

    let metrics = h.metrics_text();
    let dropped = metrics
        .lines()
        .find_map(|line| line.strip_prefix("lumen_webhook_dropped_total "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_default();
    assert!(dropped > 0, "a full queue must drop and count: {metrics}");
}

#[tokio::test]
async fn cancellation_stops_the_sender_without_waiting_for_the_receiver() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    // A receiver that never answers: only cancellation can end this delivery.
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .mount(&receiver)
        .await;

    let config = WebhookSettings {
        timeout_ms: 60_000,
        ..webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50])
    };
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let created: Value = h
        .create_key(&json!({ "name": "shutdown", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    // Wait until the delivery is actually in flight, then cancel.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while received_requests(&receiver).await.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !received_requests(&receiver).await.is_empty(),
        "the delivery should be in flight before cancelling"
    );

    let started = std::time::Instant::now();
    h.cancel.cancel();
    // Give the task a moment to observe the token, then confirm nothing new is
    // attempted and the cancel itself was instant.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation must not block on the in-flight attempt"
    );
    let after_cancel = received_requests(&receiver).await.len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        received_requests(&receiver).await.len(),
        after_cancel,
        "no further attempts after cancellation"
    );
}

// ---- Opt-in default ---------------------------------------------------------

#[tokio::test]
async fn a_gateway_without_a_webhooks_block_calls_nothing() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;

    // The same gateway assembly, minus the webhook wiring: this is what every
    // other integration test builds, and the receiver must stay untouched.
    let store = KeyStore::in_memory().await.expect("open store");
    let keys = AuthState::load(Vec::new(), Vec::new());
    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(
        metrics.clone(),
        chat_registry(&upstream.uri()),
        tokens,
        latency,
    )
    .with_pricing(dollar_pricing())
    .with_auth(runtime);
    let base = common::spawn_state(state, LIMIT).await;
    let client = reqwest::Client::new();

    let created: Value = client
        .post(format!("{base}/admin/keys"))
        .bearer_auth(master())
        .json(&json!({ "name": "quiet", "budget_max": 50.0 }))
        .send()
        .await
        .expect("create key")
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 5
        }))
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), 200);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        received_requests(&receiver).await.is_empty(),
        "with no [webhooks] block the gateway must make no outbound call"
    );
    // ...and the webhook series do not even exist.
    let text = metrics.encode_text();
    assert!(!text.contains("lumen_webhook_"), "{text}");
}

// ---- The admin surface (ADR 011 amendment) ----------------------------------

impl Harness {
    async fn get_webhooks(&self) -> reqwest::Response {
        self.client
            .get(format!("{}/admin/webhooks", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("get webhooks")
    }

    async fn put_webhooks(&self, body: &Value) -> reqwest::Response {
        self.client
            .put(format!("{}/admin/webhooks", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("put webhooks")
    }

    async fn delete_webhooks(&self) -> reqwest::Response {
        self.client
            .delete(format!("{}/admin/webhooks", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("delete webhooks")
    }

    async fn put_signing_key(&self, body: &Value) -> reqwest::Response {
        self.client
            .put(format!("{}/admin/webhooks/signing-key", self.base))
            .bearer_auth(master())
            .json(body)
            .send()
            .await
            .expect("put signing key")
    }

    async fn delete_signing_key(&self) -> reqwest::Response {
        self.client
            .delete(format!("{}/admin/webhooks/signing-key", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("delete signing key")
    }
}

/// A minimal, valid `PUT /admin/webhooks` body pointed at `receiver`.
fn put_body(receiver: &str) -> Value {
    json!({
        "url": format!("{receiver}/events"),
        "events": ["budget.threshold"],
        "thresholds": [50],
        "channel_capacity": 64,
        "timeout_ms": 2000,
        "max_attempts": 3,
        "retry_base_ms": 20
    })
}

#[tokio::test]
async fn the_webhook_routes_require_the_master_key() {
    let upstream = MockServer::start().await;
    let receiver = MockServer::start().await;
    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    for (method, route) in [
        ("GET", "admin/webhooks"),
        ("PUT", "admin/webhooks"),
        ("DELETE", "admin/webhooks"),
        ("PUT", "admin/webhooks/signing-key"),
        ("DELETE", "admin/webhooks/signing-key"),
    ] {
        let url = format!("{}/{route}", h.base);
        let build = |token: Option<String>| {
            let mut request = match method {
                "GET" => h.client.get(&url),
                "PUT" => h.client.put(&url),
                _ => h.client.delete(&url),
            }
            .json(&json!({ "url": "https://x.example/e", "secret": "s" }));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            request
        };
        assert_eq!(
            build(None).send().await.expect("send").status(),
            401,
            "{method} {route} without a token"
        );
        assert_eq!(
            build(Some("b".repeat(64)))
                .send()
                .await
                .expect("send")
                .status(),
            401,
            "{method} {route} with a wrong token"
        );
    }
}

#[tokio::test]
async fn get_webhooks_reports_the_live_settings_and_their_source() {
    let upstream = MockServer::start().await;
    let receiver = MockServer::start().await;
    let config = webhook_config(
        &receiver.uri(),
        vec![EventKind::BudgetThreshold],
        vec![50, 80],
    );
    // Sealed secret, no env var: `signed` must still be true.
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, Some(SECRET)).await;

    let body: Value = h.get_webhooks().await.json().await.expect("status json");
    assert_eq!(body["enabled"], true);
    // Applied from the "file block" by the harness, with no stored row.
    assert_eq!(body["source"], "config");
    assert_eq!(body["signed"], true);
    assert_eq!(body["signing_key_stored"], true);
    assert_eq!(body["settings"]["url"], config.url.as_str());
    assert_eq!(body["settings"]["thresholds"], json!([50, 80]));
    assert!(
        body.get("updated_at").is_none(),
        "a config-file deployment has no stored timestamp: {body}"
    );
    // The secret itself appears nowhere.
    assert!(!body.to_string().contains(SECRET), "{body}");
}

#[tokio::test]
async fn put_webhooks_replaces_every_setting_and_takes_effect_immediately() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    // Booted pointing at a receiver that would fail the assertions below...
    let old_receiver = MockServer::start().await;
    mount_receiver(&old_receiver).await;
    // ...then retargeted to this one, with a different threshold set.
    let new_receiver = MockServer::start().await;
    mount_receiver(&new_receiver).await;

    let config = webhook_config(
        &old_receiver.uri(),
        vec![EventKind::BudgetThreshold],
        vec![95],
    );
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, Some(SECRET)).await;

    let mut body = put_body(&new_receiver.uri());
    // A different capacity, so the queue (and its sender task) is rebuilt.
    body["channel_capacity"] = json!(128);
    body["events"] = json!(["budget.threshold", "key.deleted"]);
    body["thresholds"] = json!([40]);
    let response = h.put_webhooks(&body).await;
    assert_eq!(response.status(), 200);
    let status: Value = response.json().await.expect("status json");
    assert_eq!(status["enabled"], true);
    assert_eq!(status["source"], "database");
    assert_eq!(status["settings"]["channel_capacity"], 128);
    assert!(status["updated_at"].as_i64().is_some_and(|t| t > 0));

    // One $46 call against a $100 budget = 46 %, crossing the NEW 40 %
    // threshold and going to the NEW receiver.
    let created: Value = h
        .create_key(&json!({ "name": "retargeted", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    let events = wait_for_events(&new_receiver, 1).await;
    assert_eq!(
        events.len(),
        1,
        "the new receiver gets the event: {events:?}"
    );
    assert_eq!(events[0]["threshold"], 40);
    assert!(
        received_requests(&old_receiver).await.is_empty(),
        "the old receiver must get nothing after the retarget"
    );

    // Still signed, with the same sealed secret, over the rebuilt pipeline.
    let requests = received_requests(&new_receiver).await;
    assert_eq!(
        header(&requests[0], "x-lumen-signature"),
        expected_signature(SECRET, &requests[0].body)
    );

    // And the change survives a restart: it is in the database.
    let stored = h
        .store
        .load_webhook_config()
        .await
        .expect("load stored config")
        .expect("a row was written");
    assert!(stored.enabled);
    assert_eq!(stored.settings.channel_capacity, 128);
    assert_eq!(stored.settings.thresholds, [40]);
}

#[tokio::test]
async fn put_webhooks_rejects_an_invalid_document_without_touching_the_live_pipeline() {
    let upstream = MockServer::start().await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;
    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    // Field, bad value, and why: the validation is shared with the config
    // loader, so these are the same rules a bad TOML block would hit.
    let cases: [(&str, Value); 8] = [
        ("url", json!("")),
        ("url", json!("ftp://b.example/e")),
        ("url", json!(" https://b.example/e ")),
        ("events", json!([])),
        ("events", json!(["budget.almost"])),
        ("thresholds", json!([0])),
        ("channel_capacity", json!(0)),
        // A LUMEN_-prefixed variable cannot be excluded from the config
        // loader's overlay before the database is read, so the API refuses it
        // and points at the sealed-secret route instead.
        ("signing_key_env", json!("LUMEN_WEBHOOK_SECRET")),
    ];
    for (field, value) in cases {
        let mut body = put_body(&receiver.uri());
        body[field] = value.clone();
        let response = h.put_webhooks(&body).await;
        assert_eq!(response.status(), 400, "{field} = {value} must be rejected");
        let error: Value = response.json().await.expect("error json");
        assert_eq!(error["error"]["code"], "LM-1001", "{field} = {value}");
    }

    // Nothing was persisted and the boot settings are still in force.
    assert!(h.store.load_webhook_config().await.expect("load").is_none());
    let status: Value = h.get_webhooks().await.json().await.expect("status json");
    assert_eq!(status["settings"]["url"], config.url.as_str());
    assert_eq!(status["source"], "config");
}

#[tokio::test]
async fn delete_webhooks_stops_emission_and_survives_a_reload() {
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;
    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![40]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    let response = h.delete_webhooks().await;
    assert_eq!(response.status(), 200);
    let status: Value = response.json().await.expect("status json");
    assert_eq!(status["enabled"], false);
    assert_eq!(status["source"], "none");
    assert!(status.get("settings").is_none());

    // Traffic that would have crossed 40 % now signals nothing.
    let created: Value = h
        .create_key(&json!({ "name": "silenced", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received_requests(&receiver).await.is_empty());

    // The DELETE is stored as a DISABLED row, not as an absent one, so the
    // config block cannot re-enable it on the next reload.
    let stored = h
        .store
        .load_webhook_config()
        .await
        .expect("load")
        .expect("a shadow row was written");
    assert!(!stored.enabled);
    assert_eq!(stored.settings.url, config.url);

    // Re-resolving against the very same config block keeps it off.
    h.controller
        .resolve_and_apply(Some(&config), &h.auth.keys)
        .expect("resolve against the same config block");
    assert!(
        h.controller.live_settings().is_none(),
        "a disabled row must shadow the config block"
    );

    // Idempotent: a second DELETE is still a 200.
    assert_eq!(h.delete_webhooks().await.status(), 200);
}

#[tokio::test]
async fn a_put_can_enable_webhooks_on_a_gateway_that_booted_without_them() {
    // The control-plane path the amendment exists for: provision a running
    // gateway that has no [webhooks] block at all.
    let upstream = MockServer::start().await;
    mount_openai_chat(&upstream).await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;
    let h = spawn_gateway_without_webhooks(chat_registry(&upstream.uri())).await;

    // Nothing is emitting, and no webhook series exists yet.
    let status: Value = h.get_webhooks().await.json().await.expect("status json");
    assert_eq!(status["enabled"], false);
    assert_eq!(status["source"], "none");
    assert!(!h.metrics_text().contains("lumen_webhook_"));

    // Seal a secret, then turn webhooks on - both through the API only.
    assert_eq!(
        h.put_signing_key(&json!({ "secret": SECRET }))
            .await
            .status(),
        204
    );
    let mut body = put_body(&receiver.uri());
    body["thresholds"] = json!([40]);
    assert_eq!(h.put_webhooks(&body).await.status(), 200);

    let status: Value = h.get_webhooks().await.json().await.expect("status json");
    assert_eq!(status["enabled"], true);
    assert_eq!(status["source"], "database");
    assert_eq!(status["signed"], true);

    let created: Value = h
        .create_key(&json!({ "name": "provisioned", "budget_max": 100.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);

    let events = wait_for_events(&receiver, 1).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["threshold"], 40);
    let requests = received_requests(&receiver).await;
    assert_eq!(
        header(&requests[0], "x-lumen-signature"),
        expected_signature(SECRET, &requests[0].body),
        "the API-sealed secret signs deliveries"
    );
    // The collectors appeared only once webhooks were actually enabled.
    assert!(h.metrics_text().contains("lumen_webhook_sent_total 1"));
}

#[tokio::test]
async fn the_signing_key_routes_never_expose_the_secret() {
    let upstream = MockServer::start().await;
    let receiver = MockServer::start().await;
    mount_receiver(&receiver).await;
    let config = webhook_config(&receiver.uri(), vec![EventKind::BudgetThreshold], vec![50]);
    let h = spawn_gateway(chat_registry(&upstream.uri()), &config, None).await;

    assert_eq!(
        h.put_signing_key(&json!({ "secret": "" })).await.status(),
        400,
        "an empty secret is a bad request"
    );
    assert_eq!(
        h.put_signing_key(&json!({ "secret": SECRET }))
            .await
            .status(),
        204
    );

    // No route returns it, and it is not sitting in the database as plaintext.
    let status = h.get_webhooks().await.text().await.expect("status text");
    assert!(status.contains("\"signing_key_stored\":true"), "{status}");
    assert!(!status.contains(SECRET), "{status}");
    let dump = h.store.debug_dump().await.expect("dump the store");
    assert!(
        !dump.contains(SECRET),
        "the signing secret must be encrypted at rest"
    );

    // Rotation applies without a restart, and to the very next delivery.
    assert_eq!(
        h.put_signing_key(&json!({ "secret": ROTATED }))
            .await
            .status(),
        204
    );
    mount_openai_chat(&upstream).await;
    let created: Value = h
        .create_key(&json!({ "name": "rotated", "budget_max": 50.0 }))
        .await
        .json()
        .await
        .expect("created key json");
    let key = created["key"].as_str().expect("plaintext key").to_owned();
    assert_eq!(h.chat(&key).await.status(), 200);
    wait_for_events(&receiver, 1).await;
    let requests = received_requests(&receiver).await;
    assert_eq!(
        header(&requests[0], "x-lumen-signature"),
        expected_signature(ROTATED, &requests[0].body),
        "the rotated secret signs the next delivery"
    );

    // And it can be forgotten again.
    assert_eq!(h.delete_signing_key().await.status(), 204);
    let status: Value = h.get_webhooks().await.json().await.expect("status json");
    assert_eq!(status["signing_key_stored"], false);
    assert_eq!(status["signed"], false);
}
