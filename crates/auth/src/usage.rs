//! Asynchronous, batched usage logging (M5 §5.3).
//!
//! The request path calls [`UsageLogger::log`], which is a non-blocking
//! `try_send` into a **bounded** channel — the request path can NEVER block
//! or slow down because the database is busy (lesson: LiteLLM #12067). A
//! dedicated writer task drains the channel and batches `INSERT`s (default:
//! every 2 s or 500 entries, whichever comes first). A full channel drops the
//! entry; the caller counts the drop in Prometheus.

use crate::store::{KeyStore, UsageRecord};
use std::time::Duration;
use tokio::sync::mpsc;

/// Tunables for the usage writer.
#[derive(Debug, Clone, Copy)]
pub struct UsageWriterConfig {
    /// Bounded channel capacity (default 10 000).
    pub capacity: usize,
    /// Flush as soon as this many records are buffered (default 500).
    pub batch_max: usize,
    /// Flush at least this often while records are pending (default 2 s).
    pub flush_interval: Duration,
}

impl Default for UsageWriterConfig {
    fn default() -> Self {
        Self {
            capacity: 10_000,
            batch_max: 500,
            flush_interval: Duration::from_secs(2),
        }
    }
}

/// Cheap-to-clone handle the request path logs through.
#[derive(Debug, Clone)]
pub struct UsageLogger {
    tx: mpsc::Sender<UsageRecord>,
}

impl UsageLogger {
    /// Enqueue one record without ever waiting. Returns `false` when the
    /// channel is full (or the writer is gone) and the record was dropped —
    /// the caller increments `usage_log_dropped_total`.
    pub fn log(&self, record: UsageRecord) -> bool {
        self.tx.try_send(record).is_ok()
    }
}

/// Spawn the writer task. Returns the logger handle and the task's join
/// handle; the task exits (after a final flush) once every logger clone is
/// dropped, which is the graceful-shutdown drain.
#[must_use]
pub fn spawn_usage_writer(
    store: KeyStore,
    config: UsageWriterConfig,
) -> (UsageLogger, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<UsageRecord>(config.capacity.max(1));
    let batch_max = config.batch_max.max(1);
    let flush_interval = config.flush_interval;

    let handle = tokio::spawn(async move {
        let mut buffer: Vec<UsageRecord> = Vec::with_capacity(batch_max);
        let mut ticker = tokio::time::interval(flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                received = rx.recv_many(&mut buffer, batch_max) => {
                    if received == 0 {
                        break; // all senders dropped — final drain below
                    }
                    if buffer.len() >= batch_max {
                        flush(&store, &mut buffer).await;
                    }
                }
                _ = ticker.tick() => {
                    if !buffer.is_empty() {
                        flush(&store, &mut buffer).await;
                    }
                }
            }
        }
        flush(&store, &mut buffer).await;
    });

    (UsageLogger { tx }, handle)
}

/// Write one batch, clearing the buffer either way: a failed batch is
/// dropped with a warning rather than retried forever against a sick DB.
async fn flush(store: &KeyStore, buffer: &mut Vec<UsageRecord>) {
    if buffer.is_empty() {
        return;
    }
    if let Err(error) = store.insert_usage(buffer).await {
        tracing::warn!(%error, dropped = buffer.len(), "usage-log batch write failed");
    }
    buffer.clear();
}
