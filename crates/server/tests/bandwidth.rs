//! `/v1` bandwidth metering: request and response body bytes are credited to
//! the authenticated key once the response body ends or is dropped, only for
//! requests that reached an upstream attempt. Observed through the units of
//! the key's flush batch.

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::providers::Format as _;
use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewGroup, NewKey};
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

const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const LIMIT: usize = 10 * 1024 * 1024;

fn registry(upstream: &str) -> Arc<Registry> {
    let spec = ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![
            ModelSpec {
                id: "gpt".to_owned(),
                upstream_id: "gpt-4o".to_owned(),
                capabilities: vec![Capability::Chat],
                modalities: vec!["text".to_owned()],
                release_date: None,
            },
            ModelSpec {
                id: "embed-small".to_owned(),
                upstream_id: "text-embedding-3-small".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: vec!["text".to_owned()],
                release_date: None,
            },
        ],
        decisions_path: None,
        forward_unknown_fields: None,
    };
    Arc::new(
        Registry::build(vec![spec], http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    )
}

/// $1 per embedded token, so a 0.5 budget refuses any embeddings call.
fn dollar_pricing() -> CostTable {
    let toml = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "embed-small"
        capabilities = ["embed"]
        cost_per_1m_input = 1000000.0
    "#;
    let config: Config = figment::Figment::new()
        .merge(figment::providers::Toml::string(toml))
        .extract()
        .expect("valid pricing config");
    CostTable::from_config(&config)
}

struct Harness {
    base: String,
    runtime: Arc<AuthRuntime>,
    key: String,
    poor_key: String,
}

async fn harness(registry: Arc<Registry>) -> Harness {
    let store = KeyStore::in_memory().await.expect("store");
    let group = store
        .create_group(NewGroup {
            name: "lease".to_owned(),
            budget_max: Some(1000.0),
            account_ref: Some(ACCOUNT.to_owned()),
        })
        .await
        .expect("group");
    let mut keys = Vec::new();
    for (name, budget) in [("rich", None), ("poor", Some(0.5))] {
        let (plain, _) = store
            .create_key(NewKey {
                name: name.to_owned(),
                group_id: Some(group.id.clone()),
                budget_max: budget,
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
        admin_token_hash: hash_key("admin"),
        master: None,
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("tokens");
    let latency = LatencyMetrics::register(&metrics).expect("latency");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_pricing(dollar_pricing())
        .with_auth(Arc::clone(&runtime));
    let base = common::spawn_state(state, LIMIT).await;
    Harness {
        base,
        runtime,
        key: keys[0].clone(),
        poor_key: keys[1].clone(),
    }
}

impl Harness {
    /// Flush once and return the `(bytes_in, bytes_out)` it carried.
    fn take(&self) -> (i64, i64) {
        let batch = self.runtime.keys.drain_flush(1_000);
        let totals = batch.deltas().iter().fold((0, 0), |acc, d| {
            (acc.0 + d.units.bytes_in, acc.1 + d.units.bytes_out)
        });
        self.runtime.keys.commit_flush(batch);
        totals
    }

    /// Accumulate flushed bytes until `done` accepts the running total, or
    /// panic after a bounded wait (the credit lands when the body drops,
    /// possibly after the client already has the response).
    async fn wait_for(&self, done: impl Fn((i64, i64)) -> bool) -> (i64, i64) {
        let mut total = (0, 0);
        for _ in 0..200 {
            let (i, o) = self.take();
            total = (total.0 + i, total.1 + o);
            if done(total) {
                return total;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("bandwidth never reached the expected total, got {total:?}");
    }

    /// Assert nothing further is credited after a settle period.
    async fn assert_quiet(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(self.take(), (0, 0), "bytes were counted more than once");
    }
}

fn embed_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({ "model": "embed-small", "input": "abcd" })).expect("json")
}

async fn mount_embeddings(upstream: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        })))
        .mount(upstream)
        .await;
}

fn sse_frame(i: usize) -> String {
    format!(
        "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
    )
}

#[tokio::test]
async fn bandwidth_counts_json_request_and_response() {
    let upstream = MockServer::start().await;
    mount_embeddings(&upstream, 200).await;
    let h = harness(registry(&upstream.uri())).await;

    let body = embed_bytes();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let out = resp.bytes().await.expect("body").len();

    let want = (
        i64::try_from(body.len()).expect("fits"),
        i64::try_from(out).expect("fits"),
    );
    assert_eq!(h.wait_for(|t| t == want).await, want);
    h.assert_quiet().await;
}

#[tokio::test]
async fn bandwidth_counts_streamed_response() {
    let upstream = MockServer::start().await;
    let sse: String = (0..50).map(sse_frame).collect::<String>() + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&upstream)
        .await;
    let h = harness(registry(&upstream.uri())).await;

    let body = serde_json::to_vec(&json!({
        "model": "gpt",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": true
    }))
    .expect("json");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let received = resp.bytes().await.expect("body").len();
    assert!(received > 0);

    let want = (
        i64::try_from(body.len()).expect("fits"),
        i64::try_from(received).expect("fits"),
    );
    assert_eq!(h.wait_for(|t| t == want).await, want);
    h.assert_quiet().await;
}

/// A raw upstream streaming one frame every 20 ms until the peer closes.
async fn endless_sse_upstream() -> String {
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
            tokio::spawn(async move {
                let mut req = [0u8; 4096];
                let _ = socket.read(&mut req).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for i in 0.. {
                    if socket.write_all(sse_frame(i).as_bytes()).await.is_err() {
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
async fn bandwidth_counts_yielded_bytes_on_client_cancel() {
    let upstream = endless_sse_upstream().await;
    let h = harness(registry(&upstream)).await;

    let body = serde_json::to_vec(&json!({
        "model": "gpt",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": true
    }))
    .expect("json");
    let mut resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let first = resp.chunk().await.expect("chunk").expect("a first frame");
    drop(resp);

    let (bytes_in, bytes_out) = h.wait_for(|t| t.1 > 0).await;
    assert_eq!(bytes_in, i64::try_from(body.len()).expect("fits"));
    // The gateway yielded at least what the client read, and far less than
    // the endless stream would have produced (the body was dropped).
    assert!(bytes_out >= i64::try_from(first.len()).expect("fits"));
    assert!(bytes_out < 4096, "counted bytes never yielded: {bytes_out}");
    // Credited exactly once even though the stream's end never came.
    h.assert_quiet().await;
}

#[tokio::test]
async fn bandwidth_counts_upstream_error() {
    let upstream = MockServer::start().await;
    mount_embeddings(&upstream, 500).await;
    let h = harness(registry(&upstream.uri())).await;

    let body = embed_bytes();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_server_error(), "{}", resp.status());
    let out = resp.bytes().await.expect("body").len();
    assert!(out > 0);

    let want = (
        i64::try_from(body.len()).expect("fits"),
        i64::try_from(out).expect("fits"),
    );
    assert_eq!(h.wait_for(|t| t == want).await, want);
    h.assert_quiet().await;
}

#[tokio::test]
async fn bandwidth_ignores_gateway_refusals() {
    let upstream = MockServer::start().await;
    mount_embeddings(&upstream, 200).await;
    let h = harness(registry(&upstream.uri())).await;
    let client = reqwest::Client::new();

    // LM-1001: invalid body.
    let resp = client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.expect("json");
    assert_eq!(v["error"]["code"], "LM-1001");

    // LM-4001: the poor key cannot reserve the $1 estimate.
    let resp = client
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&h.poor_key)
        .header("content-type", "application/json")
        .body(embed_bytes())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 402);
    let v: Value = resp.json().await.expect("json");
    assert_eq!(v["error"]["code"], "LM-4001");

    assert!(upstream.received_requests().await.expect("reqs").is_empty());
    h.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bandwidth_counts_request_when_client_cancels_before_response() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(json!({
                    "object": "list",
                    "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
                    "model": "text-embedding-3-small",
                    "usage": { "prompt_tokens": 1, "total_tokens": 1 }
                })),
        )
        .mount(&upstream)
        .await;
    let h = harness(registry(&upstream.uri())).await;

    let body = embed_bytes();
    let result = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", h.base))
        .bearer_auth(&h.key)
        .header("content-type", "application/json")
        .body(body.clone())
        .timeout(Duration::from_millis(300))
        .send()
        .await;
    assert!(result.is_err(), "the client should have timed out");

    let want = (i64::try_from(body.len()).expect("fits"), 0);
    assert_eq!(h.wait_for(|t| t.0 > 0).await, want);
    h.assert_quiet().await;
}
