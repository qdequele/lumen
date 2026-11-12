//! Asynchronous, batched usage logging (M5 §5.3).
//!
//! The request path calls [`UsageLogger::log`], which is a non-blocking
//! `try_send` into a **bounded** channel - the request path can NEVER block
//! or slow down because the database is busy (lesson: LiteLLM #12067). A
//! dedicated writer task drains the channel and batches `INSERT`s (default:
//! every 2 s or 500 entries, whichever comes first). A full channel drops the
//! entry; the caller counts the drop in Prometheus.
//!
//! A batch the database refuses is retried once after a short pause. Rows
//! that still cannot be written are discarded (the writer never retries
//! forever against a sick DB, and never grows an unbounded backlog) and
//! reported through the [`UsageDropHook`], so the server counts them in the
//! same `lumen_usage_log_dropped_total` metric as the channel-full drops.

use crate::store::{KeyStore, UsageRecord};
use std::sync::Arc;
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

/// Pause before the single retry of a failed batch: long enough for a
/// transient `SQLITE_BUSY` (a purge or a flush holding the write lock) to
/// clear, short enough not to back the channel up.
const RETRY_PAUSE: Duration = Duration::from_millis(250);

/// Called by the writer with the number of usage rows it finally discarded
/// after a failed batch write and its retry. This crate has no metrics
/// dependency: the server passes a closure that increments
/// `lumen_usage_log_dropped_total`. Runs on the writer task, never on the
/// request path, and must not block.
pub type UsageDropHook = Arc<dyn Fn(usize) + Send + Sync>;

/// Cheap-to-clone handle the request path logs through.
#[derive(Debug, Clone)]
pub struct UsageLogger {
    tx: mpsc::Sender<UsageRecord>,
}

impl UsageLogger {
    /// Enqueue one record without ever waiting. Returns `false` when the
    /// channel is full (or the writer is gone) and the record was dropped -
    /// the caller increments `usage_log_dropped_total`.
    pub fn log(&self, record: UsageRecord) -> bool {
        self.tx.try_send(record).is_ok()
    }
}

/// Spawn the writer task. Returns the logger handle and the task's join
/// handle; the task exits (after a final flush) once every logger clone is
/// dropped, which is the graceful-shutdown drain.
///
/// Rows discarded after a failed write are only logged; use
/// [`spawn_usage_writer_with_drop_hook`] to count them.
#[must_use]
pub fn spawn_usage_writer(
    store: KeyStore,
    config: UsageWriterConfig,
) -> (UsageLogger, tokio::task::JoinHandle<()>) {
    spawn_usage_writer_with_drop_hook(store, config, Arc::new(|_| {}))
}

/// [`spawn_usage_writer`] with a hook told how many rows each failed batch
/// finally lost (after its one retry), so they are counted and not only
/// logged.
#[must_use]
pub fn spawn_usage_writer_with_drop_hook(
    store: KeyStore,
    config: UsageWriterConfig,
    on_dropped: UsageDropHook,
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
                        break; // all senders dropped - final drain below
                    }
                    if buffer.len() >= batch_max {
                        flush(&store, &mut buffer, &on_dropped).await;
                    }
                }
                _ = ticker.tick() => {
                    if !buffer.is_empty() {
                        flush(&store, &mut buffer, &on_dropped).await;
                    }
                }
            }
        }
        flush(&store, &mut buffer, &on_dropped).await;
    });

    (UsageLogger { tx }, handle)
}

/// Write one batch, clearing the buffer either way. A failed write is
/// retried once after [`RETRY_PAUSE`] (the insert is one transaction, so a
/// failed attempt wrote nothing and the retry cannot duplicate rows). A batch
/// that fails twice is discarded rather than retried forever against a sick
/// DB, and its row count is handed to `on_dropped`.
async fn flush(store: &KeyStore, buffer: &mut Vec<UsageRecord>, on_dropped: &UsageDropHook) {
    if buffer.is_empty() {
        return;
    }
    if let Err(error) = store.insert_usage(buffer).await {
        tracing::warn!(%error, rows = buffer.len(), "usage-log batch write failed; retrying once");
        tokio::time::sleep(RETRY_PAUSE).await;
        if let Err(error) = store.insert_usage(buffer).await {
            tracing::warn!(
                %error,
                dropped = buffer.len(),
                "usage-log batch write failed twice; the batch is discarded"
            );
            on_dropped(buffer.len());
        }
    }
    buffer.clear();
}
