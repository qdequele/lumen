//! Tests for the bounded-channel usage writer (M5 §5.3).

use lumen_auth::store::{KeyStore, UsageRecord};
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use std::time::Duration;

fn record(model: &str) -> UsageRecord {
    UsageRecord {
        key_id: None,
        model: model.to_owned(),
        model_used: model.to_owned(),
        capability: "chat".to_owned(),
        tokens_in: 1,
        tokens_out: 2,
        search_units: None,
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
