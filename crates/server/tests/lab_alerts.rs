//! ADR 015 amendment 2026-11-20: the `key.expiring` scan reports each
//! milestone once per key and expiry, for Lab teams' live keys only.

use lumen_auth::store::{KeyPatch, KeyStore, NewGroup, NewKey};
use lumen_server::lab_alerts::scan_key_expiry;

const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

async fn group(store: &KeyStore, account_ref: Option<&str>, parent: Option<&str>) -> String {
    store
        .create_group(NewGroup {
            name: "g".to_owned(),
            budget_max: Some(100.0),
            account_ref: account_ref.map(str::to_owned),
            parent_id: parent.map(str::to_owned),
        })
        .await
        .unwrap()
        .id
}

async fn key(store: &KeyStore, name: &str, group_id: Option<&str>, expires_at: i64) -> String {
    store
        .create_key(NewKey {
            name: name.to_owned(),
            group_id: group_id.map(str::to_owned),
            expires_at: Some(expires_at),
            external_ref: Some(format!("lab-{name}")),
            ..NewKey::default()
        })
        .await
        .unwrap()
        .1
        .id
}

/// The `key.expiring` events in the outbox, as `(key_id, days_left)`.
async fn sent(store: &KeyStore) -> Vec<(String, i64)> {
    let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM usage_outbox ORDER BY rowid")
        .fetch_all(store.pool())
        .await
        .unwrap();
    bodies
        .iter()
        .map(|b| serde_json::from_str::<serde_json::Value>(b).unwrap())
        .filter(|v| v["type"] == "key.expiring")
        .map(|v| {
            (
                v["data"]["key_id"].as_str().unwrap().to_owned(),
                v["data"]["days_left"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn each_milestone_is_reported_once_per_key_and_expiry() {
    let store = KeyStore::in_memory().await.unwrap();
    let lease = group(&store, Some(ACCOUNT), None).await;
    let k = key(&store, "ci", Some(&lease), NOW + 3 * DAY).await;

    // Three days left: the 7-day milestone, once however often it scans.
    assert_eq!(scan_key_expiry(&store, NOW * 1000).await.unwrap(), 1);
    assert_eq!(scan_key_expiry(&store, NOW * 1000).await.unwrap(), 0);
    assert_eq!(
        scan_key_expiry(&store, (NOW + DAY) * 1000).await.unwrap(),
        0
    );
    assert_eq!(sent(&store).await, [(k.clone(), 7)]);

    // Within a day, then expired: one event each.
    let near = (NOW + 2 * DAY + 3_600) * 1000;
    assert_eq!(scan_key_expiry(&store, near).await.unwrap(), 1);
    assert_eq!(scan_key_expiry(&store, near).await.unwrap(), 0);
    let after = (NOW + 3 * DAY + 60) * 1000;
    assert_eq!(scan_key_expiry(&store, after).await.unwrap(), 1);
    assert_eq!(scan_key_expiry(&store, after + 3_600_000).await.unwrap(), 0);
    assert_eq!(
        sent(&store).await,
        [(k.clone(), 7), (k.clone(), 1), (k.clone(), 0)]
    );

    // The event is the Lab's shape, with the stable id the schema defines.
    let body: String = sqlx::query_scalar("SELECT body FROM usage_outbox ORDER BY rowid LIMIT 1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["account_id"], ACCOUNT);
    assert_eq!(v["api_key_id"], "lab-ci");
    assert_eq!(v["product"], "lumen");
    assert_eq!(v["data"]["key_name"], "ci");
    assert_eq!(
        v["id"],
        lumen_auth::billing::uuid_v5(&format!(
            "key:{k}:{}:7",
            v["data"]["expires_at"].as_str().unwrap()
        ))
    );

    // A new expiry is a new key lifetime: its milestones are reported again.
    store
        .update_key(
            &k,
            KeyPatch {
                expires_at: Some(NOW + 9 * DAY),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(scan_key_expiry(&store, after).await.unwrap(), 1);
}

#[tokio::test]
async fn only_live_enabled_keys_of_lab_teams_are_scanned() {
    let store = KeyStore::in_memory().await.unwrap();
    let lease = group(&store, Some(ACCOUNT), None).await;
    let sub = group(&store, None, Some(&lease)).await;
    let operator = group(&store, Some("team-a"), None).await;

    let in_sub = key(&store, "in-sub", Some(&sub), NOW + DAY).await;
    key(&store, "no-group", None, NOW + DAY).await;
    key(&store, "operator", Some(&operator), NOW + DAY).await;
    key(&store, "far", Some(&lease), NOW + 30 * DAY).await;
    key(&store, "long-expired", Some(&lease), NOW - 10 * DAY).await;
    let disabled = key(&store, "disabled", Some(&lease), NOW + DAY).await;
    store
        .update_key(
            &disabled,
            KeyPatch {
                disabled: Some(true),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let deleted = key(&store, "deleted", Some(&lease), NOW + DAY).await;
    store.delete_key(&deleted).await.unwrap();

    assert_eq!(scan_key_expiry(&store, NOW * 1000).await.unwrap(), 1);
    assert_eq!(sent(&store).await, [(in_sub, 1)]);
}

#[tokio::test]
async fn two_replicas_scanning_at_once_report_once() {
    let store = KeyStore::in_memory().await.unwrap();
    let lease = group(&store, Some(ACCOUNT), None).await;
    key(&store, "ci", Some(&lease), NOW + DAY).await;
    let (a, b) = tokio::join!(
        scan_key_expiry(&store, NOW * 1000),
        scan_key_expiry(&store, NOW * 1000)
    );
    assert_eq!(a.unwrap() + b.unwrap(), 1);
    assert_eq!(sent(&store).await.len(), 1);
}
