//! ADR 015: the billing outbox and the atomic spend + event flush.

#![allow(clippy::float_cmp)]

use lumen_auth::store::{FlushRow, KeyStore, NewKey, OutboxInsert};
use sqlx::Row;

async fn store_with_key() -> (KeyStore, String) {
    let store = KeyStore::in_memory().await.unwrap();
    let (_, key) = store
        .create_key(NewKey {
            name: "k".to_owned(),
            ..NewKey::default()
        })
        .await
        .unwrap();
    (store, key.id)
}

async fn spend_and_watermark(store: &KeyStore, id: &str) -> (f64, i64) {
    let row = sqlx::query("SELECT budget_spent, billed_micro FROM virtual_keys WHERE id = ?")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    (row.get("budget_spent"), row.get("billed_micro"))
}

fn insert(id: &str, created_ms: i64) -> OutboxInsert {
    OutboxInsert {
        id: id.to_owned(),
        body: format!(r#"{{"id":"{id}"}}"#),
        created_ms,
    }
}

#[tokio::test]
async fn persist_flush_writes_spend_watermark_and_event_together() {
    let (store, key) = store_with_key().await;
    let rows = [FlushRow {
        key_id: key.clone(),
        spent_usd: 1.5,
        billed_micro: 1_500_000,
    }];
    store
        .persist_flush(&rows, &[insert("e1", 10)])
        .await
        .unwrap();
    assert_eq!(spend_and_watermark(&store, &key).await, (1.5, 1_500_000));
    let due = store.outbox_due(10, 100).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "e1");
    assert_eq!(due[0].body, r#"{"id":"e1"}"#);
}

#[tokio::test]
async fn persist_flush_is_atomic() {
    let (store, key) = store_with_key().await;
    // A duplicate event id violates the primary key on the second insert:
    // the whole transaction, spend included, must roll back.
    let rows = [FlushRow {
        key_id: key.clone(),
        spent_usd: 2.0,
        billed_micro: 2_000_000,
    }];
    let err = store
        .persist_flush(&rows, &[insert("dup", 1), insert("dup", 1)])
        .await;
    assert!(err.is_err());
    assert_eq!(spend_and_watermark(&store, &key).await, (0.0, 0));
    assert!(store.outbox_due(i64::MAX, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn due_rows_come_oldest_first_and_respect_the_limit() {
    let (store, _) = store_with_key().await;
    store
        .persist_flush(&[], &[insert("b", 20), insert("a", 10), insert("c", 30)])
        .await
        .unwrap();
    let due = store.outbox_due(100, 2).await.unwrap();
    let ids: Vec<&str> = due.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);
}

#[tokio::test]
async fn delivered_rows_are_never_due_again_and_purge_after_the_window() {
    let (store, _) = store_with_key().await;
    store
        .persist_flush(&[], &[insert("a", 10), insert("b", 10)])
        .await
        .unwrap();
    assert_eq!(
        store
            .outbox_mark_delivered(&["a".to_owned()], 50)
            .await
            .unwrap(),
        1
    );
    let due = store.outbox_due(100, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "b");
    assert_eq!(store.outbox_stats().await.unwrap(), (1, Some(10)));
    // Purge only touches delivered rows older than the cutoff.
    assert_eq!(store.outbox_purge_delivered(49).await.unwrap(), 0);
    assert_eq!(store.outbox_purge_delivered(51).await.unwrap(), 1);
    assert_eq!(store.outbox_stats().await.unwrap(), (1, Some(10)));
}

#[tokio::test]
async fn reschedule_backs_off_exponentially_and_caps_at_five_minutes() {
    let (store, _) = store_with_key().await;
    store.persist_flush(&[], &[insert("a", 0)]).await.unwrap();
    let ids = ["a".to_owned()];
    // First failure: 2 s (+ jitter).
    store.outbox_reschedule(&ids, 1_000, 7).await.unwrap();
    assert!(store.outbox_due(3_006, 10).await.unwrap().is_empty());
    assert_eq!(store.outbox_due(3_007, 10).await.unwrap().len(), 1);
    // Many failures later the delay is capped at 300 s.
    for _ in 0..20 {
        store.outbox_reschedule(&ids, 1_000, 0).await.unwrap();
    }
    assert!(store.outbox_due(300_999, 10).await.unwrap().is_empty());
    assert_eq!(store.outbox_due(301_000, 10).await.unwrap().len(), 1);
}
