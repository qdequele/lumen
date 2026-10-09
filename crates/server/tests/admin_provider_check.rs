//! End-to-end tests for `POST /admin/providers/{name}/check`: the
//! master-key-gated, on-demand provider key check. The upstream is wiremock;
//! the check must hit only the provider's free key-check endpoint (never an
//! inference route), report a rejected key as data in a 200 (never as a
//! misleading gateway 401), and never echo the key.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::auth::AuthRuntime;
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;
const PROVIDER_KEY: &str = "sk-provider-secret-e2e";

fn master() -> String {
    "a".repeat(64)
}

fn registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some(PROVIDER_KEY.to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gpt".to_owned(),
            upstream_id: "gpt-4o".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
            release_date: None,
        }],
        decisions_path: None,
        forward_unknown_fields: None,
    }];
    Arc::new(
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    )
}

async fn spawn_gateway(registry: Arc<Registry>) -> String {
    let store = KeyStore::in_memory().await.expect("open store");
    let groups = store.load_groups().await.expect("load groups");
    let entries = store.load_auth_entries().await.expect("load entries");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(groups, entries),
        store,
        admin_token_hash: hash_key(&master()),
        master: None,
    });
    let state = common::base_state(registry).with_auth(runtime);
    common::spawn_state(state, LIMIT).await
}

async fn post_check(base: &str, name: &str, token: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().post(format!("{base}/admin/providers/{name}/check"));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req.send().await.expect("send")
}

/// Mount `/models` answering `status`, and an inference route that must
/// never be called.
async fn mount_upstream(upstream: &MockServer, status: u16) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header(
            "authorization",
            format!("Bearer {PROVIDER_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn check_requires_the_master_key() {
    let upstream = MockServer::start().await;
    let base = spawn_gateway(registry(&upstream.uri())).await;

    assert_eq!(post_check(&base, "openai", None).await.status(), 401);
    assert_eq!(
        post_check(&base, "openai", Some(&"b".repeat(64)))
            .await
            .status(),
        401
    );
    assert!(upstream
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

#[tokio::test]
async fn accepted_key_reports_valid_without_an_inference_call() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream, 200).await;
    let base = spawn_gateway(registry(&upstream.uri())).await;

    let resp = post_check(&base, "openai", Some(&master())).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.expect("body");
    assert!(!text.contains(PROVIDER_KEY), "key leaked: {text}");
    let body: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(body["provider"], "openai");
    assert_eq!(body["kind"], "openai");
    assert_eq!(body["key_valid"], true);
    assert_eq!(body["reachable"], true);
    assert_eq!(body["http_status"], 200);
    assert_eq!(
        body["endpoint"],
        format!("GET {}/models", upstream.uri()).as_str()
    );
    assert!(body["latency_ms"].is_u64());
    assert!(body["detail"].is_string());
}

#[tokio::test]
async fn rejected_key_is_data_in_a_200_not_a_gateway_401() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream, 401).await;
    let base = spawn_gateway(registry(&upstream.uri())).await;

    let resp = post_check(&base, "openai", Some(&master())).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.expect("body");
    assert!(!text.contains(PROVIDER_KEY), "key leaked: {text}");
    let body: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(body["key_valid"], false);
    assert_eq!(body["http_status"], 401);
}

#[tokio::test]
async fn unknown_provider_is_404() {
    let upstream = MockServer::start().await;
    let base = spawn_gateway(registry(&upstream.uri())).await;

    let resp = post_check(&base, "nope", Some(&master())).await;
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "LM-1003");
}
