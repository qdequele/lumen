//! ADR 020: sub-groups through the admin API. A sub-group is created under
//! a top-level group (scoped to its account on a scoped call), takes the
//! parent's account, never changes parent, blocks its parent's deletion, has
//! no team key routes of its own (`admin_byok.rs`), and is enforced live with no restart.

mod common;

use lumen_auth::key::hash_key;
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::KeyStore;
use lumen_core::{BudgetScope, GatewayError};
use lumen_server::auth::AuthRuntime;
use lumen_server::state::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Arc;

const MASTER: &str = "admin-master-token";
const A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";
const HEADER: &str = "X-Lumen-Account-Ref";

async fn spawn() -> (String, Arc<AuthRuntime>) {
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: KeyStore::in_memory().await.unwrap(),
        admin_token_hash: hash_key(MASTER),
        master: None,
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).unwrap();
    let latency = LatencyMetrics::register(&metrics).unwrap();
    let state = AppState::new(metrics, common::empty_registry(), tokens, latency)
        .with_auth(Arc::clone(&runtime));
    (common::spawn_state(state, 1 << 20).await, runtime)
}

async fn call(
    base: &str,
    method: Method,
    path: &str,
    scope: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(MASTER);
    if let Some(scope) = scope {
        req = req.header(HEADER, scope);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A lease (top-level group) of `account` with `budget_max` USD; its id.
async fn lease(base: &str, account: &str, budget_max: f64) -> String {
    let (status, group) = call(
        base,
        Method::POST,
        "/admin/groups",
        None,
        Some(json!({"name": format!("lab-{account}"), "budget_max": budget_max, "account_ref": account})),
    )
    .await;
    assert_eq!(status, 201, "{group}");
    group["id"].as_str().unwrap().to_owned()
}

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_scoped_sub_group_inherits_the_account_and_is_enforced_live() {
    let (base, runtime) = spawn().await;
    let team = lease(&base, A, 1.0).await;

    let (status, sub) = call(
        &base,
        Method::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "staging", "budget_max": 50.0, "parent_id": team})),
    )
    .await;
    assert_eq!(status, 201, "{sub}");
    assert_eq!(sub["parent_id"], team.as_str());
    assert_eq!(sub["account_ref"], A);
    let sub_id = sub["id"].as_str().unwrap().to_owned();

    // The account sees it, filtered by parent too; another account does not.
    let (_, listed) = call(
        &base,
        Method::GET,
        &format!("/admin/groups?parent_id={team}"),
        Some(A),
        None,
    )
    .await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [sub_id.as_str()]);
    let (status, _) = call(
        &base,
        Method::GET,
        &format!("/admin/groups/{sub_id}"),
        Some(B),
        None,
    )
    .await;
    assert_eq!(status, 404);

    // A key created in the sub-group is bounded by the $1 lease at once.
    let (status, key) = call(
        &base,
        Method::POST,
        "/admin/keys",
        Some(A),
        Some(json!({"name": "ci", "group_id": sub_id})),
    )
    .await;
    assert_eq!(status, 201, "{key}");
    let entry = runtime
        .keys
        .authenticate(key["key"].as_str().unwrap(), 1_800_000_000)
        .expect("live key");
    assert_eq!(entry.root_group_id().as_deref(), Some(team.as_str()));
    assert!(matches!(
        entry.admit(1_800_000_000, 1, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
}

#[tokio::test]
async fn another_accounts_parent_is_the_unknown_id_404() {
    let (base, _) = spawn().await;
    let other = lease(&base, B, 5.0).await;
    let (status, body) = call(
        &base,
        Method::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "sneaky", "parent_id": other})),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "LM-1003");
}

#[tokio::test]
async fn sub_group_rules_are_400() {
    let (base, _) = spawn().await;
    let team = lease(&base, A, 5.0).await;
    let (_, sub) = call(
        &base,
        Method::POST,
        "/admin/groups",
        None,
        Some(json!({"name": "sub", "parent_id": team})),
    )
    .await;
    let sub_id = sub["id"].as_str().unwrap().to_owned();

    let cases = [
        // One level deep.
        (
            Method::POST,
            "/admin/groups".to_owned(),
            json!({"name": "deep", "parent_id": sub_id}),
        ),
        // The account is the parent's.
        (
            Method::POST,
            "/admin/groups".to_owned(),
            json!({"name": "x", "parent_id": team, "account_ref": A}),
        ),
        // The parent never changes.
        (
            Method::PATCH,
            format!("/admin/groups/{sub_id}"),
            json!({"parent_id": null}),
        ),
        // Billing fields are the parent's.
        (
            Method::PATCH,
            format!("/admin/groups/{sub_id}"),
            json!({"bandwidth_micro_usd_per_gb": 10}),
        ),
        (
            Method::PATCH,
            format!("/admin/groups/{team}"),
            json!({"account_ref": null}),
        ),
        // An unknown parent, unscoped.
        (
            Method::POST,
            "/admin/groups".to_owned(),
            json!({"name": "x", "parent_id": "nope"}),
        ),
    ];
    for (method, path, body) in cases {
        let (status, answer) = call(&base, method.clone(), &path, None, Some(body.clone())).await;
        assert_eq!(status, 400, "{method} {path} {body}: {answer}");
        assert_eq!(answer["error"]["code"], "LM-1001", "{answer}");
    }

    // The parent cannot go while the sub-group draws from it.
    let (status, answer) = call(
        &base,
        Method::DELETE,
        &format!("/admin/groups/{team}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 400, "{answer}");
    assert!(message(&answer).contains("sub-group"), "{answer}");
    let (status, _) = call(
        &base,
        Method::DELETE,
        &format!("/admin/groups/{sub_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &base,
        Method::DELETE,
        &format!("/admin/groups/{team}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn the_parent_view_counts_sub_group_spend() {
    let (base, runtime) = spawn().await;
    let team = lease(&base, A, 10.0).await;
    let (_, sub) = call(
        &base,
        Method::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "sub", "parent_id": team})),
    )
    .await;
    let (_, key) = call(
        &base,
        Method::POST,
        "/admin/keys",
        Some(A),
        Some(json!({"name": "k", "group_id": sub["id"]})),
    )
    .await;
    let entry = runtime
        .keys
        .authenticate(key["key"].as_str().unwrap(), 1_800_000_000)
        .unwrap();
    entry
        .admit(1_800_000_000, 1, 2_500)
        .unwrap()
        .settle(2_500, 1);

    // What the Lab's lease sync reads includes the sub-group's spend.
    let (status, view) = call(
        &base,
        Method::GET,
        &format!("/admin/groups/{team}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["spent_micro"], 2_500);
    assert_eq!(view["parent_id"], Value::Null);
}
