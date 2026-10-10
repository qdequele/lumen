//! Platform contract v2 section 8.3: per-account scoping of the admin
//! surface through `X-Lumen-Account-Ref`, and the group view the Lab's
//! lease sync reads (section 8.2).

mod common;

use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, UsageRecord};
use lumen_server::auth::AuthRuntime;
use lumen_server::state::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
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

/// One admin call; `scope` sets the header.
async fn call(
    base: &str,
    method: reqwest::Method,
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

/// Two accounts with one group and one key each, plus an operator key with
/// no group. Returns `(group_a, key_a, group_b, key_b, loose)` ids.
async fn seed(base: &str) -> (String, String, String, String, String) {
    let mut ids = Vec::new();
    for account in [A, B] {
        let (status, group) = call(
            base,
            reqwest::Method::POST,
            "/admin/groups",
            None,
            Some(json!({"name": account, "budget_max": 5.0, "account_ref": account})),
        )
        .await;
        assert_eq!(status, 201, "{group}");
        let gid = group["id"].as_str().unwrap().to_owned();
        let (status, key) = call(
            base,
            reqwest::Method::POST,
            "/admin/keys",
            None,
            Some(json!({"name": "k", "group_id": gid})),
        )
        .await;
        assert_eq!(status, 201, "{key}");
        ids.push(gid);
        ids.push(key["id"].as_str().unwrap().to_owned());
    }
    let (status, loose) = call(
        base,
        reqwest::Method::POST,
        "/admin/keys",
        None,
        Some(json!({"name": "operator"})),
    )
    .await;
    assert_eq!(status, 201, "{loose}");
    let loose = loose["id"].as_str().unwrap().to_owned();
    (
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
        loose,
    )
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn lists_are_filtered_to_the_scoped_account() {
    let (base, _) = spawn().await;
    let (group_a, key_a, _, _, _) = seed(&base).await;
    let (_, keys) = call(&base, reqwest::Method::GET, "/admin/keys", Some(A), None).await;
    assert_eq!(ids(&keys), std::slice::from_ref(&key_a));
    let (_, groups) = call(&base, reqwest::Method::GET, "/admin/groups", Some(A), None).await;
    assert_eq!(ids(&groups), std::slice::from_ref(&group_a));
    // A conflicting query filter cannot widen the scope.
    let (status, groups) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups?account_ref={B}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{groups}");
    assert_eq!(ids(&groups), Vec::<String>::new());
    // Tombstones stay inside the scope too.
    let (_, keys) = call(
        &base,
        reqwest::Method::GET,
        "/admin/keys?include_deleted=true",
        Some(A),
        None,
    )
    .await;
    assert_eq!(ids(&keys), std::slice::from_ref(&key_a));
    // Unscoped: everything, as before.
    let (_, keys) = call(&base, reqwest::Method::GET, "/admin/keys", None, None).await;
    assert_eq!(ids(&keys).len(), 3);
    let (_, groups) = call(&base, reqwest::Method::GET, "/admin/groups", None, None).await;
    assert_eq!(ids(&groups).len(), 2);
}

#[tokio::test]
async fn another_accounts_key_or_group_is_404() {
    use reqwest::Method as M;
    let (base, _) = spawn().await;
    let (_, key_a, group_b, key_b, loose) = seed(&base).await;
    let patch = json!({"name": "renamed"});
    let grant = json!({"amount": 1.0});
    for (method, path, body) in [
        (
            M::PATCH,
            format!("/admin/keys/{key_b}"),
            Some(patch.clone()),
        ),
        (M::POST, format!("/admin/keys/{key_b}/rotate"), None),
        (
            M::POST,
            format!("/admin/keys/{key_b}/grant"),
            Some(grant.clone()),
        ),
        (M::DELETE, format!("/admin/keys/{key_b}"), None),
        (
            M::PATCH,
            format!("/admin/keys/{loose}"),
            Some(patch.clone()),
        ),
        (M::POST, format!("/admin/keys/{loose}/rotate"), None),
        (M::DELETE, format!("/admin/keys/{loose}"), None),
        (M::GET, format!("/admin/groups/{group_b}"), None),
        (
            M::PATCH,
            format!("/admin/groups/{group_b}"),
            Some(patch.clone()),
        ),
        (
            M::POST,
            format!("/admin/groups/{group_b}/grant"),
            Some(grant.clone()),
        ),
        (M::DELETE, format!("/admin/groups/{group_b}"), None),
    ] {
        let (status, body) = call(&base, method.clone(), &path, Some(A), body).await;
        assert_eq!(status, 404, "{method} {path}: {body}");
        assert_eq!(body["error"]["code"], "LM-1003", "{method} {path}");
    }
    // A foreign id answers exactly like an id that never existed.
    let (_, foreign) = call(
        &base,
        M::PATCH,
        &format!("/admin/keys/{key_b}"),
        Some(A),
        Some(patch.clone()),
    )
    .await;
    let (_, unknown) = call(
        &base,
        M::PATCH,
        "/admin/keys/no-such-key",
        Some(A),
        Some(patch.clone()),
    )
    .await;
    assert_eq!(
        foreign["error"]["type"], unknown["error"]["type"],
        "{foreign} vs {unknown}"
    );
    assert_eq!(
        foreign["error"]["message"]
            .as_str()
            .map(|m| m.replace(&key_b, "ID")),
        unknown["error"]["message"]
            .as_str()
            .map(|m| m.replace("no-such-key", "ID")),
    );
    // Nothing was touched: B's key still exists and keeps its name.
    let (_, keys) = call(&base, M::GET, "/admin/keys", Some(B), None).await;
    assert_eq!(keys[0]["name"], "k", "{keys}");
    // The same calls on A's own key succeed, and unscoped calls see B.
    let (status, _) = call(
        &base,
        M::PATCH,
        &format!("/admin/keys/{key_a}"),
        Some(A),
        Some(patch.clone()),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        &base,
        M::PATCH,
        &format!("/admin/keys/{key_b}"),
        None,
        Some(patch),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn creating_inside_the_scope() {
    use reqwest::Method as M;
    let (base, _) = spawn().await;
    let (group_a, key_a, group_b, _, _) = seed(&base).await;
    // A key needs a group of the account.
    let (status, body) = call(
        &base,
        M::POST,
        "/admin/keys",
        Some(A),
        Some(json!({"name": "k2"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("LM-1001")),
        "{body}"
    );
    let (status, _) = call(
        &base,
        M::POST,
        "/admin/keys",
        Some(A),
        Some(json!({"name": "k2", "group_id": group_b})),
    )
    .await;
    assert_eq!(status, 404);
    let (status, body) = call(
        &base,
        M::POST,
        "/admin/keys",
        Some(A),
        Some(json!({"name": "k2", "group_id": group_a})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["group_id"], group_a);
    // A group gets the header's account_ref; a different one is refused.
    let (status, body) = call(
        &base,
        M::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "g2"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["account_ref"], A);
    let (status, _) = call(
        &base,
        M::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "g3", "account_ref": B})),
    )
    .await;
    assert_eq!(status, 400);
    // A scoped key cannot leave its account, a scoped group cannot change it.
    let (status, _) = call(
        &base,
        M::PATCH,
        &format!("/admin/keys/{key_a}"),
        Some(A),
        Some(json!({"group_id": null})),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = call(
        &base,
        M::PATCH,
        &format!("/admin/keys/{key_a}"),
        Some(A),
        Some(json!({"group_id": group_b})),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &base,
        M::PATCH,
        &format!("/admin/groups/{group_a}"),
        Some(A),
        Some(json!({"account_ref": B})),
    )
    .await;
    assert_eq!(status, 400);
    // None of the refused writes landed: A's key is still in A's group.
    let (_, keys) = call(&base, M::GET, "/admin/keys", Some(A), None).await;
    assert!(
        keys.as_array()
            .unwrap()
            .iter()
            .all(|k| k["group_id"] == group_a),
        "{keys}"
    );
}

#[tokio::test]
async fn platform_routes_are_403_with_the_header() {
    use reqwest::Method as M;
    let (base, _) = spawn().await;
    for (method, path) in [
        (M::GET, "/admin/webhooks"),
        (M::PUT, "/admin/webhooks"),
        (M::DELETE, "/admin/webhooks"),
        (M::PUT, "/admin/webhooks/signing-key"),
        (M::DELETE, "/admin/webhooks/signing-key"),
        (M::PUT, "/admin/provider-keys/openai"),
        (M::POST, "/admin/providers/openai/check"),
        (M::GET, "/admin/config"),
        (M::PUT, "/admin/config"),
        (M::GET, "/admin/config/providers"),
        (M::GET, "/admin/config/providers/openai"),
        (M::GET, "/admin/config/virtual_models"),
        (M::GET, "/admin/config/virtual_models/x"),
        (M::GET, "/admin/config/virtual_models/x/plan"),
        (M::GET, "/admin/config/resilience"),
    ] {
        let (status, body) = call(&base, method.clone(), path, Some(A), None).await;
        assert_eq!(status, 403, "{method} {path}: {body}");
        assert_eq!(body["error"]["code"], "LM-4005", "{method} {path}");
        assert_eq!(body["error"]["type"], "invalid_request", "{method} {path}");
    }
    let (status, _) = call(&base, M::GET, "/admin/webhooks", None, None).await;
    assert_ne!(status, 403, "unscoped calls are unchanged");
}

#[tokio::test]
async fn a_malformed_header_is_400_and_no_key_is_401_first() {
    let (base, _) = spawn().await;
    for bad in ["acme", "", "0192f3c1-7c2e-7b1a-9f00"] {
        let (status, body) =
            call(&base, reqwest::Method::GET, "/admin/keys", Some(bad), None).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (400, Some("LM-1001")),
            "{bad:?}: {body}"
        );
    }
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/config"))
        .header(HEADER, A)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "the master key is checked before the scope"
    );
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/keys"))
        .header(HEADER, "acme")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "the master key is checked before the header"
    );
}

#[tokio::test]
async fn get_group_returns_live_micro_figures_for_the_lease_sync() {
    let (base, runtime) = spawn().await;
    let (group_a, _, _, _, _) = seed(&base).await;
    // A member key spends in memory (no flush): the view must be live.
    let (plain, record) = runtime
        .store
        .create_key(lumen_auth::store::NewKey {
            name: "spender".to_owned(),
            group_id: Some(group_a.clone()),
            ..lumen_auth::store::NewKey::default()
        })
        .await
        .unwrap();
    runtime
        .keys
        .upsert_group(&runtime.store.get_group(&group_a).await.unwrap().unwrap());
    runtime.keys.upsert(hash_key(plain.reveal()), &record);
    let entry = runtime.keys.authenticate(plain.reveal(), 1).unwrap();
    entry.admit(1, 0, 250_000).unwrap().settle(250_000, 3);
    // Live figures, before any flush.
    let (status, view) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups/{group_a}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["id"], group_a);
    assert_eq!(view["account_ref"], A);
    assert_eq!(view["budget_max_micro"], 5_000_000);
    assert_eq!(view["spent_micro"], 250_000);
    // The lease sync sets the cap absolutely, then raises it relatively.
    let (status, patched) = call(
        &base,
        reqwest::Method::PATCH,
        &format!("/admin/groups/{group_a}"),
        Some(A),
        Some(json!({"budget_max": 7.5})),
    )
    .await;
    assert_eq!(status, 200, "{patched}");
    let (_, view) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups/{group_a}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(view["budget_max_micro"], 7_500_000);
    let (status, granted) = call(
        &base,
        reqwest::Method::POST,
        &format!("/admin/groups/{group_a}/grant"),
        Some(A),
        Some(json!({"amount": 0.5})),
    )
    .await;
    assert_eq!(status, 200, "{granted}");
    let (_, view) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups/{group_a}"),
        None,
        None,
    )
    .await;
    assert_eq!(view["budget_max_micro"], 8_000_000, "{view}");
    // Unscoped, an unknown id is the usual 404.
    let (status, body) = call(
        &base,
        reqwest::Method::GET,
        "/admin/groups/no-such-group",
        None,
        None,
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("LM-1003"))
    );
}

#[tokio::test]
async fn a_capless_group_view_reports_a_null_cap() {
    let (base, _) = spawn().await;
    let (status, group) = call(
        &base,
        reqwest::Method::POST,
        "/admin/groups",
        Some(A),
        Some(json!({"name": "open"})),
    )
    .await;
    assert_eq!(status, 201, "{group}");
    let gid = group["id"].as_str().unwrap();
    let (status, view) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups/{gid}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["budget_max_micro"], Value::Null);
    assert_eq!(view["spent_micro"], 0);
}

#[tokio::test]
async fn a_scoped_delete_retry_still_evicts_an_own_zombie_key() {
    let (base, runtime) = spawn().await;
    let (group_a, _, group_b, _, _) = seed(&base).await;
    // Two live keys whose DB rows are already tombstoned, as after a client
    // disconnect between the DB write and the in-memory eviction.
    let mut zombies = Vec::new();
    for group in [&group_a, &group_b] {
        let (plain, record) = runtime
            .store
            .create_key(lumen_auth::store::NewKey {
                name: "zombie".to_owned(),
                group_id: Some(group.clone()),
                ..lumen_auth::store::NewKey::default()
            })
            .await
            .unwrap();
        runtime.keys.upsert(hash_key(plain.reveal()), &record);
        runtime.store.delete_key(&record.id).await.unwrap().unwrap();
        zombies.push((plain, record.id));
    }
    // A's retry repairs A's zombie (and still answers 404, like unscoped).
    let (status, _) = call(
        &base,
        reqwest::Method::DELETE,
        &format!("/admin/keys/{}", zombies[0].1),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 404);
    assert!(runtime
        .keys
        .authenticate(zombies[0].0.reveal(), 1)
        .is_none());
    // A cannot touch B's zombie: 404, and B's live entry is left alone.
    let (status, _) = call(
        &base,
        reqwest::Method::DELETE,
        &format!("/admin/keys/{}", zombies[1].1),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 404);
    assert!(runtime
        .keys
        .authenticate(zombies[1].0.reveal(), 1)
        .is_some());
}

#[tokio::test]
async fn usage_report_and_export_are_scoped() {
    let (base, runtime) = spawn().await;
    let (group_a, key_a, group_b, key_b, _) = seed(&base).await;
    let row = |key: &str, group: Option<&str>| UsageRecord {
        key_id: Some(key.to_owned()),
        group_id: group.map(str::to_owned),
        model: "gpt".to_owned(),
        model_used: "gpt".to_owned(),
        route: None,
        provider: "openai".to_owned(),
        capability: "chat".to_owned(),
        tokens_in: 1,
        tokens_out: 2,
        search_units: None,
        cached_tokens: None,
        reasoning_tokens: None,
        cache_write_tokens: None,
        media_count: 0,
        media_bytes: 0,
        estimated: false,
        cost: 0.5,
        latency_ms: 5,
        status: 200,
        metadata: None,
        ts: 1_000,
    };
    runtime
        .store
        .insert_usage(&[
            row(&key_a, Some(&group_a)),
            row(&key_b, Some(&group_b)),
            row("operator", None),
        ])
        .await
        .unwrap();
    let (status, report) = call(
        &base,
        reqwest::Method::GET,
        "/admin/usage?since=0&until=2000&group_by=total",
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["groups"][0]["requests"], 1, "{report}");
    // A filter naming B's group cannot widen the scope either.
    let (status, report) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/usage?since=0&until=2000&group_by=total&group_id={group_b}"),
        Some(A),
        None,
    )
    .await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["groups"], json!([]), "{report}");
    let (_, report) = call(
        &base,
        reqwest::Method::GET,
        "/admin/usage?since=0&until=2000&group_by=total",
        None,
        None,
    )
    .await;
    assert_eq!(report["groups"][0]["requests"], 3, "{report}");
    let (_, page) = call(
        &base,
        reqwest::Method::GET,
        "/admin/usage/export?since=0&until=2000",
        Some(A),
        None,
    )
    .await;
    assert_eq!(page["rows"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["rows"][0]["group_id"], group_a);
    let (_, page) = call(
        &base,
        reqwest::Method::GET,
        "/admin/usage/export?since=0&until=2000",
        None,
        None,
    )
    .await;
    assert_eq!(page["rows"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn the_account_header_is_a_canonical_lowercase_uuid() {
    let (base, _) = spawn().await;
    let (group_a, key_a, _, _, _) = seed(&base).await;
    let upper = A.to_ascii_uppercase();
    assert_ne!(upper, A);
    // An uppercase header sees exactly the rows the lowercase one sees.
    for path in ["/admin/keys", "/admin/groups"] {
        let (status, lower_rows) = call(&base, reqwest::Method::GET, path, Some(A), None).await;
        assert_eq!(status, 200, "{path}: {lower_rows}");
        let (status, upper_rows) =
            call(&base, reqwest::Method::GET, path, Some(&upper), None).await;
        assert_eq!(status, 200, "{path}: {upper_rows}");
        assert_eq!(ids(&upper_rows), ids(&lower_rows), "{path}");
    }
    let (_, keys) = call(
        &base,
        reqwest::Method::GET,
        "/admin/keys",
        Some(&upper),
        None,
    )
    .await;
    assert_eq!(ids(&keys), std::slice::from_ref(&key_a));
    let (status, group) = call(
        &base,
        reqwest::Method::GET,
        &format!("/admin/groups/{group_a}"),
        Some(&upper),
        None,
    )
    .await;
    assert_eq!(status, 200, "{group}");
    // A scoped create under an uppercase header stores the lowercase ref,
    // whether the ref is forced or echoed in the body.
    for body in [
        json!({"name": "forced", "budget_max": 1.0}),
        json!({"name": "echoed", "budget_max": 1.0, "account_ref": upper}),
    ] {
        let (status, created) = call(
            &base,
            reqwest::Method::POST,
            "/admin/groups",
            Some(&upper),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, 201, "{body}: {created}");
        let id = created["id"].as_str().unwrap();
        let (status, stored) = call(
            &base,
            reqwest::Method::GET,
            &format!("/admin/groups/{id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, 200, "{stored}");
        assert_eq!(stored["account_ref"], A, "{body}: {stored}");
    }
}
