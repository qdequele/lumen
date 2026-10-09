//! ADR 015: the shared budget flush persists spend and enqueues billing
//! events in one transaction, on every flush path.

#![allow(clippy::float_cmp)]

use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewGroup, NewKey};
use lumen_server::auth::AuthRuntime;
use lumen_server::budget_flush::{flush_budgets, retire_and_flush_key};
use sqlx::Row;
use std::sync::Arc;

const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

/// A runtime with one key in a Lab-linked group, loaded as boot does.
async fn runtime(billing: bool, in_group: bool) -> (Arc<AuthRuntime>, String, String) {
    let store = KeyStore::in_memory().await.unwrap();
    let group = store
        .create_group(NewGroup {
            name: "lease".to_owned(),
            budget_max: Some(10.0),
            account_ref: Some(ACCOUNT.to_owned()),
        })
        .await
        .unwrap();
    let (plain, _) = store
        .create_key(NewKey {
            name: "k".to_owned(),
            group_id: in_group.then(|| group.id.clone()),
            external_ref: Some("lab-key".to_owned()),
            ..NewKey::default()
        })
        .await
        .unwrap();
    let keys = AuthState::load(
        store.load_groups().await.unwrap(),
        store.load_auth_entries().await.unwrap(),
    );
    if billing {
        keys.set_billing(Some(Arc::new(BillingPolicy {
            source: "eu-1".to_owned(),
        })));
    }
    let runtime = Arc::new(AuthRuntime {
        keys,
        store,
        admin_token_hash: hash_key("admin"),
        master: None,
    });
    (runtime, plain.reveal().to_owned(), group.id)
}

fn spend(runtime: &AuthRuntime, plain: &str, cost: i64) {
    let entry = runtime.keys.authenticate(plain, 1).unwrap();
    entry.admit(1, 0, cost).unwrap().settle(cost, 3);
}

async fn watermark(runtime: &AuthRuntime) -> (f64, i64) {
    let row = sqlx::query("SELECT budget_spent, billed_micro FROM virtual_keys")
        .fetch_one(runtime.store.pool())
        .await
        .unwrap();
    (row.get("budget_spent"), row.get("billed_micro"))
}

#[tokio::test]
async fn a_flush_persists_spend_and_enqueues_one_event() {
    let (rt, plain, _) = runtime(true, true).await;
    spend(&rt, &plain, 250_000);
    assert!(flush_budgets(&rt).await);
    assert_eq!(watermark(&rt).await, (0.25, 250_000));
    let due = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&due[0].body).unwrap();
    assert_eq!(body["account_id"], ACCOUNT);
    assert_eq!(body["data"]["provider_cost_micro_usd"], 250_000);
    // Nothing new spent: the next flush writes nothing.
    assert!(flush_budgets(&rt).await);
    assert_eq!(rt.store.outbox_due(i64::MAX, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn without_usage_events_no_event_is_written() {
    let (rt, plain, _) = runtime(false, true).await;
    spend(&rt, &plain, 250_000);
    assert!(flush_budgets(&rt).await);
    assert_eq!(watermark(&rt).await, (0.25, 250_000));
    assert_eq!(
        rt.store.outbox_due(i64::MAX, 10).await.unwrap(),
        [] as [lumen_auth::store::OutboxRow; 0]
    );
}

#[tokio::test]
async fn joining_a_billable_group_bills_only_new_spend() {
    let (rt, plain, group_id) = runtime(true, false).await;
    spend(&rt, &plain, 900_000);
    assert!(flush_budgets(&rt).await); // operator key: watermark catches up
    let key_id = rt.store.list_keys(false).await.unwrap()[0].id.clone();
    let patch: lumen_auth::store::KeyPatch =
        serde_json::from_str(&format!(r#"{{"group_id":"{group_id}"}}"#)).unwrap();
    let updated = rt.store.update_key(&key_id, patch).await.unwrap().unwrap();
    rt.keys.apply(&updated);
    spend(&rt, &plain, 100_000);
    assert!(flush_budgets(&rt).await);
    let due = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&due[0].body).unwrap();
    assert_eq!(body["data"]["provider_cost_micro_usd"], 100_000);
}

#[tokio::test]
async fn deleting_a_key_bills_its_last_delta() {
    let (rt, plain, _) = runtime(true, true).await;
    spend(&rt, &plain, 70_000);
    let key_id = rt.store.list_keys(false).await.unwrap()[0].id.clone();
    let entry = rt.keys.remove(&key_id).unwrap();
    retire_and_flush_key(&rt, entry).await;
    assert_eq!(rt.store.outbox_due(i64::MAX, 10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_flushes_bill_a_delta_once() {
    let (rt, plain, _) = runtime(true, true).await;
    spend(&rt, &plain, 250_000);
    // A retired key is drained by every flush whether or not it is dirty, so
    // two unserialized flushes would both bill its delta.
    let key_id = rt.store.list_keys(false).await.unwrap()[0].id.clone();
    let evicted = rt.keys.remove(&key_id).unwrap();
    rt.keys.retire(evicted);
    let (a, b) = (Arc::clone(&rt), Arc::clone(&rt));
    let first = tokio::spawn(async move { flush_budgets(&a).await });
    let second = tokio::spawn(async move { flush_budgets(&b).await });
    first.await.unwrap();
    second.await.unwrap();
    let due = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
    assert_eq!(due.len(), 1, "two overlapping flushes bill one delta");
    let body: serde_json::Value = serde_json::from_str(&due[0].body).unwrap();
    assert_eq!(body["data"]["provider_cost_micro_usd"], 250_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_delete_flush_loses_nothing() {
    let (rt, plain_a, group_id) = runtime(true, true).await;
    let (plain_b, record_b) = rt
        .store
        .create_key(NewKey {
            name: "b".to_owned(),
            group_id: Some(group_id),
            external_ref: Some("lab-key-b".to_owned()),
            ..NewKey::default()
        })
        .await
        .unwrap();
    rt.keys.upsert(hash_key(plain_b.reveal()), &record_b);
    spend(&rt, &plain_a, 250_000);
    spend(&rt, plain_b.reveal(), 100_000);
    let key_a = rt.keys.authenticate(&plain_a, 1).unwrap().id().to_owned();
    let evicted = rt.keys.remove(&key_a).unwrap();

    // Another flusher holds the guard, so the delete's flush parks.
    let guard = rt.keys.flush_guard().await;
    let rt2 = Arc::clone(&rt);
    let delete = tokio::spawn(async move { retire_and_flush_key(&rt2, evicted).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The client goes away: the handler future is dropped mid-flush.
    delete.abort();
    let _ = delete.await;
    drop(guard);

    // The detached flush still runs to completion on its own: both deltas are
    // billed without any other flusher being needed.
    let mut due = Vec::new();
    for _ in 0..100 {
        due = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
        if due.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // A later flush bills nothing again.
    assert!(flush_budgets(&rt).await);
    let again = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
    assert_eq!(again.len(), due.len(), "nothing is billed twice");
    let mut costs: Vec<i64> = due
        .iter()
        .map(|row| {
            let body: serde_json::Value = serde_json::from_str(&row.body).unwrap();
            body["data"]["provider_cost_micro_usd"].as_i64().unwrap()
        })
        .collect();
    costs.sort_unstable();
    assert_eq!(
        costs,
        vec![100_000, 250_000],
        "each delta billed exactly once"
    );
}

#[tokio::test]
async fn a_request_in_flight_on_a_deleted_key_is_still_billed() {
    let (rt, plain, _) = runtime(true, true).await;
    let entry = rt.keys.authenticate(&plain, 1).unwrap();
    let reservation = entry.admit(1, 0, 100_000).unwrap();
    let key_id = rt.store.list_keys(false).await.unwrap()[0].id.clone();
    let evicted = rt.keys.remove(&key_id).unwrap();
    retire_and_flush_key(&rt, evicted).await;
    assert_eq!(
        rt.store.outbox_due(i64::MAX, 10).await.unwrap(),
        [] as [lumen_auth::store::OutboxRow; 0]
    );
    // The request finishes after the delete; its settled cost is billed.
    reservation.settle(40_000, 3);
    drop(entry);
    assert!(flush_budgets(&rt).await);
    let due = rt.store.outbox_due(i64::MAX, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&due[0].body).unwrap();
    assert_eq!(body["data"]["provider_cost_micro_usd"], 40_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flush_that_waited_for_the_guard_stamps_the_time_it_ran() {
    let (rt, plain, _) = runtime(true, true).await;
    spend(&rt, &plain, 250_000);
    // Another flusher holds the guard: this flush parks behind it.
    let guard = rt.keys.flush_guard().await;
    let rt2 = Arc::clone(&rt);
    let flush = tokio::spawn(async move { flush_budgets(&rt2).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let released_ms = lumen_auth::now_unix_ms();
    drop(guard);
    assert!(flush.await.unwrap());
    let created_ms: i64 = sqlx::query("SELECT created_ms FROM usage_outbox")
        .fetch_one(rt.store.pool())
        .await
        .unwrap()
        .get("created_ms");
    assert!(
        created_ms >= released_ms,
        "the event is stamped {}ms before the flush could run",
        released_ms - created_ms
    );
}
