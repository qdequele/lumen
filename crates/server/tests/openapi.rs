//! `GET /openapi.json`: the gateway's contract, served to the master key
//! only, and only when auth is enabled (like the rest of the admin surface).

mod common;

use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_server::auth::AuthRuntime;
use lumen_server::state::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use std::sync::Arc;

const MASTER: &str = "admin-master-token";

async fn spawn_with_auth() -> String {
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: KeyStore::in_memory().await.unwrap(),
        admin_token_hash: hash_key(MASTER),
        master: None,
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).unwrap();
    let latency = LatencyMetrics::register(&metrics).unwrap();
    let state =
        AppState::new(metrics, common::empty_registry(), tokens, latency).with_auth(runtime);
    common::spawn_state(state, 1 << 20).await
}

#[tokio::test]
async fn the_master_key_gets_the_document_as_json() {
    let base = spawn_with_auth().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/openapi.json"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(doc["paths"]["/openapi.json"]["get"].is_object());
    assert!(doc["paths"]["/v1/chat/completions"]["post"].is_object());
}

#[tokio::test]
async fn a_scoped_call_is_403() {
    let base = spawn_with_auth().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/openapi.json"))
        .bearer_auth(MASTER)
        .header(
            "X-Lumen-Account-Ref",
            "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-4005");
}

#[tokio::test]
async fn without_the_master_key_it_is_401_and_without_auth_404() {
    let base = spawn_with_auth().await;
    let resp = reqwest::get(format!("{base}/openapi.json")).await.unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-4004");

    let open = common::spawn().await;
    let resp = reqwest::get(format!("{open}/openapi.json")).await.unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1003");
}

/// A virtual key is not the master key: `/openapi.json` refuses it exactly
/// like `/admin/keys` does (same status, same code).
#[tokio::test]
async fn a_virtual_key_gets_the_admin_rejection() {
    let base = spawn_with_auth().await;
    let client = reqwest::Client::new();
    let minted: Value = client
        .post(format!("{base}/admin/keys"))
        .bearer_auth(MASTER)
        .json(&json!({"name": "app"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = minted["key"].as_str().unwrap().to_owned();

    let mut seen = Vec::new();
    for path in ["/openapi.json", "/admin/keys"] {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        seen.push((status, body["error"]["code"].clone()));
    }
    assert_eq!(seen[0], (401, json!("LM-4004")));
    assert_eq!(seen[0], seen[1]);
}
