//! An admin JSON body that runs over `body_limit` while it is read (chunked,
//! no `Content-Length`) is `413 LM-1002` quoting the configured limit, as on
//! `/v1/*`: never a `400 LM-1001`.

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
const LIMIT: usize = 1024;

async fn spawn() -> String {
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
    common::spawn_state(state, LIMIT).await
}

/// Send `body` chunked (no `Content-Length`), so only the handler's
/// extractor finds it over the limit.
async fn send_chunked(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    body: &str,
) -> (u16, Value) {
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = body
        .as_bytes()
        .chunks(512)
        .map(|c| Ok(c.to_vec()))
        .collect();
    let resp = client
        .request(method, url)
        .bearer_auth(MASTER)
        .header("content-type", "application/json")
        .header("if-match", "any")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

#[tokio::test]
async fn an_over_limit_chunked_admin_body_is_a_413_lm_1002() {
    use reqwest::Method;

    let base = spawn().await;
    let client = reqwest::Client::new();

    // The team key routes check their group before they read the body.
    let group: Value = client
        .post(format!("{base}/admin/groups"))
        .bearer_auth(MASTER)
        .json(&json!({ "name": "team" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let group = group["id"].as_str().unwrap();

    // Valid JSON, far over the 1 KiB limit.
    let body = json!({ "name": "a".repeat(64 * 1024) }).to_string();
    let routes = [
        (Method::POST, "/admin/keys".to_owned()),
        (Method::PATCH, "/admin/keys/key_x".to_owned()),
        (Method::POST, "/admin/keys/key_x/grant".to_owned()),
        (Method::POST, "/admin/groups".to_owned()),
        (Method::PATCH, format!("/admin/groups/{group}")),
        (Method::POST, format!("/admin/groups/{group}/grant")),
        (Method::PUT, format!("/admin/groups/{group}/byok/openai")),
        (
            Method::POST,
            format!("/admin/groups/{group}/byok/openai/keys"),
        ),
        (Method::PUT, "/admin/provider-keys/openai".to_owned()),
        (Method::POST, "/admin/platform-keys/openai/keys".to_owned()),
        (
            Method::PATCH,
            "/admin/platform-keys/openai/keys/ppk_x".to_owned(),
        ),
        (Method::PUT, "/admin/webhooks".to_owned()),
        (Method::PUT, "/admin/webhooks/signing-key".to_owned()),
        (Method::PUT, "/admin/config/providers/openai".to_owned()),
        (Method::PUT, "/admin/config/virtual_models/vm".to_owned()),
    ];
    for (method, route) in routes {
        let label = format!("{method} {route}");
        let (status, json) = send_chunked(&client, method, format!("{base}{route}"), &body).await;
        assert_eq!(status, 413, "{label}: {json}");
        assert_eq!(json["error"]["code"], "LM-1002", "{label}: {json}");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("1024"),
            "{label}: {json}"
        );
    }
}

/// The mapping is only for the size: a malformed body is still `LM-1001`.
#[tokio::test]
async fn a_malformed_admin_body_is_still_lm_1001() {
    let base = spawn().await;
    let client = reqwest::Client::new();
    let (status, json) = send_chunked(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/keys"),
        "{ not json",
    )
    .await;
    assert_eq!(status, 400, "{json}");
    assert_eq!(json["error"]["code"], "LM-1001", "{json}");
}
