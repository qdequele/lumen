//! ADR 015: account_ref / external_ref on the admin API.

mod common;

use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_server::auth::AuthRuntime;
use lumen_server::budget_flush::flush_budgets;
use lumen_server::state::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use std::sync::Arc;

const MASTER: &str = "admin-master-token";
const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const OTHER_ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";

async fn spawn(billing: bool) -> (String, Arc<AuthRuntime>) {
    let store = KeyStore::in_memory().await.unwrap();
    let keys = AuthState::load(Vec::new(), Vec::new());
    if billing {
        keys.set_billing(Some(Arc::new(BillingPolicy {
            source: "eu-1".to_owned(),
        })));
    }
    let runtime = Arc::new(AuthRuntime {
        keys,
        store,
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
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(MASTER);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Settle `cost_micro` of spend on the key behind `plaintext`.
fn spend(runtime: &AuthRuntime, plaintext: &str, cost_micro: i64) {
    let entry = runtime.keys.authenticate(plaintext, 1).unwrap();
    entry.admit(1, 0, cost_micro).unwrap().settle(cost_micro, 3);
}

/// Every queued usage event as `(account_id, cost_micro_usd)`, sorted (the
/// handler's flush stamps the wall clock, the test's flush a synthetic one, so
/// creation order is not meaningful here).
async fn events(runtime: &AuthRuntime) -> Vec<(String, i64)> {
    let mut found: Vec<(String, i64)> = runtime
        .store
        .outbox_due(i64::MAX, 100)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            let body: Value = serde_json::from_str(&row.body).unwrap();
            (
                body["account_id"].as_str().unwrap().to_owned(),
                body["data"]["cost_micro_usd"].as_i64().unwrap(),
            )
        })
        .collect();
    found.sort();
    found
}

async fn create_group(base: &str, account: &str) -> String {
    let (s, group) = call(
        base,
        reqwest::Method::POST,
        "/admin/groups",
        Some(json!({"name": "lease", "budget_max": 5.0, "account_ref": account})),
    )
    .await;
    assert_eq!(s, 201);
    group["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn refs_are_set_listed_filtered_and_cleared() {
    let (base, _) = spawn(true).await;
    let (s, group) = call(
        &base,
        reqwest::Method::POST,
        "/admin/groups",
        Some(json!({"name":"lease","budget_max":5.0,"account_ref":ACCOUNT})),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(group["account_ref"], ACCOUNT);
    let gid = group["id"].as_str().unwrap();
    let (s, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":gid,"external_ref":"lab-key-1"})),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(key["external_ref"], "lab-key-1");
    assert!(key.get("billed_micro").is_none());
    call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"other"})),
    )
    .await;

    let (_, only) = call(
        &base,
        reqwest::Method::GET,
        "/admin/keys?external_ref=lab-key-1",
        None,
    )
    .await;
    assert_eq!(only.as_array().unwrap().len(), 1);
    let (_, groups) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups?account_ref={ACCOUNT}"),
        None,
    )
    .await;
    assert_eq!(groups.as_array().unwrap().len(), 1);

    let kid = key["id"].as_str().unwrap();
    let (s, cleared) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"external_ref":null})),
    )
    .await;
    assert_eq!(s, 200);
    assert!(cleared["external_ref"].is_null());
}

#[tokio::test]
async fn account_ref_must_be_a_uuid_only_when_billing_is_on() {
    let (billed, _) = spawn(true).await;
    let (s, err) = call(
        &billed,
        reqwest::Method::POST,
        "/admin/groups",
        Some(json!({"name":"g","account_ref":"acme"})),
    )
    .await;
    assert_eq!(s, 400);
    assert_eq!(err["error"]["code"], "LM-1001");
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("account_ref"));

    let (plain, _) = spawn(false).await;
    let (s, _) = call(
        &plain,
        reqwest::Method::POST,
        "/admin/groups",
        Some(json!({"name":"g","account_ref":"acme"})),
    )
    .await;
    assert_eq!(s, 201);
}

#[tokio::test]
async fn refs_are_bounded_and_non_empty() {
    let (base, _) = spawn(false).await;
    for bad in [json!(""), json!("x".repeat(129))] {
        let (s, _) = call(
            &base,
            reqwest::Method::POST,
            "/admin/keys",
            Some(json!({"name":"k","external_ref":bad})),
        )
        .await;
        assert_eq!(s, 400);
        let (s, _) = call(
            &base,
            reqwest::Method::POST,
            "/admin/groups",
            Some(json!({"name":"g","account_ref":bad})),
        )
        .await;
        assert_eq!(s, 400);
    }
}

#[tokio::test]
async fn patching_refs_validates_them_too() {
    let (base, runtime) = spawn(true).await;
    let gid = create_group(&base, ACCOUNT).await;
    let other = create_group(&base, OTHER_ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":gid})),
    )
    .await;
    let kid = key["id"].as_str().unwrap();
    // Settled spend is waiting to be flushed: a rejected patch must not
    // flush it.
    spend(&runtime, key["key"].as_str().unwrap(), 250_000);

    let (s, err) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{gid}"),
        Some(json!({"account_ref":"acme"})),
    )
    .await;
    assert_eq!(s, 400);
    assert_eq!(err["error"]["code"], "LM-1001");
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"group_id": other, "external_ref":""})),
    )
    .await;
    assert_eq!(s, 400);
    assert!(
        events(&runtime).await.is_empty(),
        "a rejected patch never flushes"
    );

    flush_budgets(&runtime, 10_000).await;
    assert_eq!(events(&runtime).await, vec![(ACCOUNT.to_owned(), 250_000)]);
}

#[tokio::test]
async fn refs_are_counted_in_characters_not_bytes() {
    let (base, _) = spawn(false).await;
    // 128 two-byte characters: 256 bytes, but within the 128-character limit.
    let (s, _) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","external_ref":"é".repeat(128)})),
    )
    .await;
    assert_eq!(s, 201);
    let (s, _) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","external_ref":"é".repeat(129)})),
    )
    .await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn a_patch_that_cannot_change_billability_does_not_flush() {
    let (base, runtime) = spawn(true).await;
    let gid = create_group(&base, ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":gid})),
    )
    .await;
    let kid = key["id"].as_str().unwrap();
    spend(&runtime, key["key"].as_str().unwrap(), 250_000);
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"budget_max": 9.0})),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{gid}"),
        Some(json!({"budget_max": 9.0})),
    )
    .await;
    assert_eq!(s, 200);
    assert!(events(&runtime).await.is_empty());
}

/// Make every outbox insert fail, so a flush with billable spend fails.
async fn break_outbox(runtime: &AuthRuntime) {
    sqlx::query(
        "CREATE TRIGGER fail_outbox BEFORE INSERT ON usage_outbox \
         BEGIN SELECT RAISE(ABORT, 'test'); END",
    )
    .execute(runtime.store.pool())
    .await
    .unwrap();
}

async fn repair_outbox(runtime: &AuthRuntime) {
    sqlx::query("DROP TRIGGER fail_outbox")
        .execute(runtime.store.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_pre_change_flush_blocks_the_key_move() {
    let (base, runtime) = spawn(true).await;
    let g1 = create_group(&base, ACCOUNT).await;
    let g2 = create_group(&base, OTHER_ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":g1})),
    )
    .await;
    let plain = key["key"].as_str().unwrap();
    let kid = key["id"].as_str().unwrap();
    spend(&runtime, plain, 250_000);

    break_outbox(&runtime).await;
    let (s, err) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"group_id": g2})),
    )
    .await;
    assert_eq!(s, 500);
    assert_eq!(err["error"]["code"], "LM-5001");
    let stored = runtime.store.list_keys(false).await.unwrap();
    assert_eq!(stored[0].group_id.as_deref(), Some(g1.as_str()));

    repair_outbox(&runtime).await;
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"group_id": g2})),
    )
    .await;
    assert_eq!(s, 200);
    flush_budgets(&runtime, 10_000).await;
    assert_eq!(
        events(&runtime).await,
        vec![(ACCOUNT.to_owned(), 250_000)],
        "the retry bills the prior spend to the old account"
    );
}

#[tokio::test]
async fn a_failed_pre_change_flush_blocks_the_account_ref_change() {
    let (base, runtime) = spawn(true).await;
    let g1 = create_group(&base, ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":g1})),
    )
    .await;
    spend(&runtime, key["key"].as_str().unwrap(), 250_000);

    break_outbox(&runtime).await;
    let (s, err) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{g1}"),
        Some(json!({"account_ref": OTHER_ACCOUNT})),
    )
    .await;
    assert_eq!(s, 500);
    assert_eq!(err["error"]["code"], "LM-5001");
    let stored = runtime.store.list_groups(false).await.unwrap();
    assert_eq!(stored[0].account_ref.as_deref(), Some(ACCOUNT));

    repair_outbox(&runtime).await;
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{g1}"),
        Some(json!({"account_ref": OTHER_ACCOUNT})),
    )
    .await;
    assert_eq!(s, 200);
    flush_budgets(&runtime, 10_000).await;
    assert_eq!(events(&runtime).await, vec![(ACCOUNT.to_owned(), 250_000)]);
}

#[tokio::test]
async fn moving_a_key_bills_prior_spend_to_the_old_account() {
    let (base, runtime) = spawn(true).await;
    let g1 = create_group(&base, ACCOUNT).await;
    let g2 = create_group(&base, OTHER_ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":g1})),
    )
    .await;
    let plain = key["key"].as_str().unwrap();
    let kid = key["id"].as_str().unwrap();

    spend(&runtime, plain, 250_000);
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/keys/{kid}"),
        Some(json!({"group_id": g2})),
    )
    .await;
    assert_eq!(s, 200);
    flush_budgets(&runtime, 10_000).await;
    assert_eq!(
        events(&runtime).await,
        vec![(ACCOUNT.to_owned(), 250_000)],
        "prior spend bills to the old account, none to the new one"
    );

    spend(&runtime, plain, 100_000);
    flush_budgets(&runtime, 20_000).await;
    assert_eq!(
        events(&runtime).await,
        vec![
            (ACCOUNT.to_owned(), 250_000),
            (OTHER_ACCOUNT.to_owned(), 100_000)
        ]
    );
}

#[tokio::test]
async fn changing_a_group_account_ref_bills_prior_spend_to_the_old_account() {
    let (base, runtime) = spawn(true).await;
    let g1 = create_group(&base, ACCOUNT).await;
    let (_, key) = call(
        &base,
        reqwest::Method::POST,
        "/admin/keys",
        Some(json!({"name":"k","group_id":g1})),
    )
    .await;
    let plain = key["key"].as_str().unwrap();

    spend(&runtime, plain, 250_000);
    let (s, _) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{g1}"),
        Some(json!({"account_ref": OTHER_ACCOUNT})),
    )
    .await;
    assert_eq!(s, 200);
    flush_budgets(&runtime, 10_000).await;
    assert_eq!(events(&runtime).await, vec![(ACCOUNT.to_owned(), 250_000)]);

    spend(&runtime, plain, 100_000);
    flush_budgets(&runtime, 20_000).await;
    assert_eq!(
        events(&runtime).await,
        vec![
            (ACCOUNT.to_owned(), 250_000),
            (OTHER_ACCOUNT.to_owned(), 100_000)
        ]
    );
}
