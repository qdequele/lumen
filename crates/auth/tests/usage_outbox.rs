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

/// `EXPLAIN QUERY PLAN` detail lines for `sql` (bind placeholders as 0).
async fn plan(store: &KeyStore, sql: &str, binds: usize) -> String {
    let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
    for _ in 0..binds {
        query = query.bind(0_i64);
    }
    query
        .fetch_all(store.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn the_sender_queries_only_walk_pending_rows() {
    // The same SQL as `outbox_due` and `outbox_stats`: delivered rows pile up
    // for 7 days before the purge, so both must reach the pending rows
    // through `idx_usage_outbox_due` (`delivered_ms IS NULL` is an equality
    // on its first column), never by scanning the table. A partial index on
    // the pending rows was measured and not chosen by the planner over this
    // one, so it would only add write cost.
    let (store, _) = store_with_key().await;
    let due = plan(
        &store,
        "SELECT id, body FROM usage_outbox \
         WHERE delivered_ms IS NULL AND next_attempt_ms <= ? \
         ORDER BY created_ms, id LIMIT ?",
        2,
    )
    .await;
    assert!(
        due.contains("USING INDEX idx_usage_outbox_due (delivered_ms=? AND next_attempt_ms<?)"),
        "outbox_due: {due}"
    );
    let stats = plan(
        &store,
        "SELECT COUNT(*) AS n, MIN(created_ms) AS oldest FROM usage_outbox WHERE delivered_ms IS NULL",
        0,
    )
    .await;
    assert!(
        stats.contains("USING INDEX idx_usage_outbox_due (delivered_ms=?)"),
        "outbox_stats: {stats}"
    );
    // One purge chunk (`outbox_purge_delivered`): a range on the same index.
    let purge = plan(
        &store,
        "DELETE FROM usage_outbox WHERE rowid IN (\
           SELECT rowid FROM usage_outbox \
           WHERE delivered_ms IS NOT NULL AND delivered_ms < ? LIMIT ?)",
        2,
    )
    .await;
    assert!(
        purge.contains("COVERING INDEX idx_usage_outbox_due (delivered_ms>? AND delivered_ms<?)"),
        "outbox_purge_delivered: {purge}"
    );
}

#[tokio::test]
async fn the_purge_deletes_in_chunks_until_the_old_delivered_rows_are_gone() {
    let (store, _) = store_with_key().await;
    // 12 001 delivered rows past the cutoff (three chunks of 5000, the last
    // partial), one delivered row inside retention, one pending row.
    sqlx::query(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 12001) \
         INSERT INTO usage_outbox (id, body, created_ms, next_attempt_ms, delivered_ms) \
         SELECT 'old-' || x, '{}', x, x, 100 FROM n",
    )
    .execute(store.pool())
    .await
    .unwrap();
    store
        .persist_flush(&[], &[insert("recent", 1), insert("pending", 1)])
        .await
        .unwrap();
    store
        .outbox_mark_delivered(&["recent".to_owned()], 500)
        .await
        .unwrap();
    assert_eq!(store.outbox_purge_delivered(200).await.unwrap(), 12_001);
    let left: Vec<String> = sqlx::query("SELECT id FROM usage_outbox ORDER BY id")
        .fetch_all(store.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| row.get("id"))
        .collect();
    assert_eq!(left, ["pending", "recent"]);
    assert_eq!(store.outbox_purge_delivered(200).await.unwrap(), 0);
}
