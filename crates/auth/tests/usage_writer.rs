//! Tests for the bounded-channel usage writer (M5 §5.3).

use lumen_auth::store::{KeyStore, UsageRecord};
use lumen_auth::usage::{
    spawn_usage_writer, spawn_usage_writer_with_drop_hook, UsageDropHook, UsageWriterConfig,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn record(model: &str) -> UsageRecord {
    UsageRecord {
        key_id: None,
        group_id: None,
        model: model.to_owned(),
        model_used: model.to_owned(),
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
        cost: 0.0,
        latency_ms: 5,
        status: 200,
        metadata: None,
        ts: 42,
    }
}

/// Poll until the usage table holds `expected` rows (bounded wait). Real
/// time on purpose: paused tokio time interacts badly with sqlx's blocking
/// SQLite I/O (the paused clock races past the pool-acquire deadline).
async fn wait_for_count(store: &KeyStore, expected: i64) {
    for _ in 0..200 {
        if store.count_usage().await.expect("count") == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "expected {expected} usage rows, still at {} after 5s",
        store.count_usage().await.expect("count")
    );
}

#[tokio::test]
async fn records_are_flushed_on_the_interval() {
    let store = KeyStore::in_memory().await.expect("store");
    let (logger, _writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 100,
            batch_max: 500,
            flush_interval: Duration::from_millis(50),
        },
    );

    assert!(logger.log(record("a")));
    assert!(logger.log(record("b")));
    wait_for_count(&store, 2).await;
}

#[tokio::test]
async fn a_full_batch_flushes_before_the_interval() {
    let store = KeyStore::in_memory().await.expect("store");
    let (logger, _writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 100,
            batch_max: 3,
            flush_interval: Duration::from_secs(3600), // interval effectively off
        },
    );

    for i in 0..3 {
        assert!(logger.log(record(&format!("m{i}"))));
    }
    // Only the batch-size trigger can flush here.
    wait_for_count(&store, 3).await;
}

#[tokio::test]
async fn full_channel_drops_instead_of_blocking() {
    let store = KeyStore::in_memory().await.expect("store");
    // Deliberately jam the writer by never yielding to it: log() must stay
    // non-blocking and simply report the drop.
    let (logger, _writer) = spawn_usage_writer(
        store,
        UsageWriterConfig {
            capacity: 2,
            batch_max: 500,
            flush_interval: Duration::from_secs(3600),
        },
    );

    assert!(logger.log(record("kept-1")));
    assert!(logger.log(record("kept-2")));
    // Channel full - dropped, and we got an immediate answer (criterion 4).
    assert!(!logger.log(record("dropped")));
}

#[tokio::test]
async fn shutdown_drains_pending_records() {
    let store = KeyStore::in_memory().await.expect("store");
    let (logger, writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 100,
            batch_max: 500,
            flush_interval: Duration::from_secs(3600),
        },
    );

    assert!(logger.log(record("pending")));
    drop(logger); // last sender gone → writer drains and exits
    writer.await.expect("writer task completes");
    assert_eq!(store.count_usage().await.expect("count"), 1);
}

/// Make every `usage_log` insert fail until the trigger is dropped.
async fn refuse_usage_inserts(store: &KeyStore) {
    sqlx::query(
        "CREATE TRIGGER refuse_usage BEFORE INSERT ON usage_log \
         BEGIN SELECT RAISE(ABORT, 'usage insert refused'); END",
    )
    .execute(store.pool())
    .await
    .expect("create trigger");
}

/// A drop hook and the counter it feeds.
fn counting_hook() -> (UsageDropHook, Arc<AtomicUsize>) {
    let dropped = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&dropped);
    let hook: UsageDropHook = Arc::new(move |rows| {
        sink.fetch_add(rows, Ordering::SeqCst);
    });
    (hook, dropped)
}

#[tokio::test]
async fn a_batch_that_fails_twice_is_counted_as_dropped() {
    let store = KeyStore::in_memory().await.expect("store");
    refuse_usage_inserts(&store).await;
    let (hook, dropped) = counting_hook();
    let (logger, writer) = spawn_usage_writer_with_drop_hook(
        store.clone(),
        UsageWriterConfig {
            capacity: 100,
            batch_max: 500,
            flush_interval: Duration::from_secs(3600),
        },
        hook,
    );

    assert!(logger.log(record("lost-1")));
    assert!(logger.log(record("lost-2")));
    drop(logger); // the shutdown drain is the flush under test
    writer.await.expect("writer task completes");

    assert_eq!(
        dropped.load(Ordering::SeqCst),
        2,
        "rows the writer discards must reach the dropped counter, not only a log line"
    );
    assert_eq!(store.count_usage().await.expect("count"), 0);
}

#[tokio::test]
async fn a_failed_batch_is_retried_once_before_being_dropped() {
    let store = KeyStore::in_memory().await.expect("store");
    refuse_usage_inserts(&store).await;
    let (hook, dropped) = counting_hook();
    let (logger, _writer) = spawn_usage_writer_with_drop_hook(
        store.clone(),
        UsageWriterConfig {
            capacity: 100,
            batch_max: 1, // flush on the first record, no interval involved
            flush_interval: Duration::from_secs(3600),
        },
        hook,
    );

    assert!(logger.log(record("retried")));
    // The first attempt fails right away; the store recovers inside the
    // writer's retry pause (250 ms), so the retry lands the row.
    tokio::time::sleep(Duration::from_millis(80)).await;
    sqlx::query("DROP TRIGGER refuse_usage")
        .execute(store.pool())
        .await
        .expect("drop trigger");

    wait_for_count(&store, 1).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 0, "nothing was lost");
}
