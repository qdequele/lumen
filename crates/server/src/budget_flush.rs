//! The one budget flush (ADR 009, 015): memory to SQLite for key spend, the
//! billing watermark and the usage events that bill it, then group pool
//! spend. Called by the periodic flush task, the shutdown drain and the
//! key-delete final flush, so the three can never diverge. Every flush runs
//! under the flush guard, because the billing watermark only moves at commit.
//! Never on the request path; failures are logged and retried by the next
//! flush.

use crate::auth::AuthRuntime;
use lumen_auth::state::{FlushBatch, KeyEntry};
use std::sync::Arc;

/// Flush every dirty and retired key, then every dirty group.
///
/// The clock is read after the flush guard is acquired, so a flush that
/// waited behind another one stamps its events with the time it actually
/// ran, and the stamps of successive flushes never go backwards.
///
/// Returns `true` when the key batch was persisted and committed (or there
/// was nothing to flush), `false` when serialization or the store failed (the
/// batch is rolled back first, so the next flush retries it). The group flush
/// does not affect billing, so its failure is logged but never changes the
/// return value. Callers that must not proceed with unbilled spend (admin
/// changes of billability) fail closed on `false`.
#[must_use]
pub async fn flush_budgets(runtime: &AuthRuntime) -> bool {
    let _guard = runtime.keys.flush_guard().await;
    let now_ms = lumen_auth::now_unix_ms();
    let batch = runtime.keys.drain_flush(now_ms);
    let persisted = persist(runtime, batch, now_ms).await;
    let groups = runtime.keys.drain_dirty_groups();
    if !groups.is_empty() {
        if let Err(error) = runtime.store.persist_group_budgets(&groups).await {
            tracing::warn!(%error, "group budget flush failed; will retry next interval");
        }
    }
    persisted
}

/// Hand a key just evicted from the live table (admin delete) to the flusher
/// and flush now, so its last spend is persisted and billed. A failed flush
/// leaves it retired, and every later flush retries it.
///
/// The retire is synchronous; the flush itself is [`flush_detached`].
pub async fn retire_and_flush_key(runtime: &Arc<AuthRuntime>, entry: Arc<KeyEntry>) {
    runtime.keys.retire(entry);
    // A failed flush is not fatal here: the key stays retired and the next
    // flush retries it, so the result is deliberately discarded.
    let _ = flush_detached(runtime).await;
}

/// Run one [`flush_budgets`] in a detached task and await it.
///
/// The spawn makes the flush uncancellable by a dropped caller (client
/// disconnect, request timeout, shutdown deadline): dropping the future
/// cannot cancel a flush between `drain_flush` and its commit or rollback,
/// which would lose dirty flags and units or double-bill a delta. Admin
/// handlers call it before a change that alters billability, so spend
/// already settled is billed under the account in force when it was spent.
///
/// Returns the flush result (see [`flush_budgets`]), or `false` when the task
/// panicked or was cancelled.
#[must_use]
pub async fn flush_detached(runtime: &Arc<AuthRuntime>) -> bool {
    let detached = Arc::clone(runtime);
    let handle = tokio::spawn(async move { flush_budgets(&detached).await });
    match handle.await {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::warn!(%error, "budget flush task failed");
            false
        }
    }
}

/// Persist one drained batch, then commit it on success or roll it back on
/// any failure; `true` means committed. The caller holds the flush guard.
async fn persist(runtime: &AuthRuntime, batch: FlushBatch, now_ms: i64) -> bool {
    if batch.is_empty() {
        runtime.keys.commit_flush(batch);
        return true;
    }
    let events = match batch.outbox_inserts(now_ms) {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, "budget flush: could not serialize usage events; will retry");
            runtime.keys.rollback_flush(batch);
            return false;
        }
    };
    match runtime.store.persist_flush(&batch.rows(), &events).await {
        Ok(()) => {
            runtime.keys.commit_flush(batch);
            true
        }
        Err(error) => {
            tracing::warn!(%error, "budget flush failed; will retry next interval");
            runtime.keys.rollback_flush(batch);
            false
        }
    }
}
