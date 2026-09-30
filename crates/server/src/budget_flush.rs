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
pub async fn flush_budgets(runtime: &AuthRuntime, now_ms: i64) {
    let _guard = runtime.keys.flush_guard().await;
    let batch = runtime.keys.drain_flush(now_ms);
    persist(runtime, batch, now_ms).await;
    let groups = runtime.keys.drain_dirty_groups();
    if !groups.is_empty() {
        if let Err(error) = runtime.store.persist_group_budgets(&groups).await {
            tracing::warn!(%error, "group budget flush failed; will retry next interval");
        }
    }
}

/// Hand a key just evicted from the live table (admin delete) to the flusher
/// and flush now, so its last spend is persisted and billed. A failed flush
/// leaves it retired, and every later flush retries it.
pub async fn retire_and_flush_key(runtime: &AuthRuntime, entry: Arc<KeyEntry>, now_ms: i64) {
    runtime.keys.retire(entry);
    flush_budgets(runtime, now_ms).await;
}

/// Persist one drained batch, then commit it on success or roll it back on
/// any failure. The caller holds the flush guard.
async fn persist(runtime: &AuthRuntime, batch: FlushBatch, now_ms: i64) {
    if batch.is_empty() {
        runtime.keys.commit_flush(batch);
        return;
    }
    let source = runtime
        .keys
        .billing()
        .map(|policy| policy.source.clone())
        .unwrap_or_default();
    let events = match batch.outbox_inserts(&source, now_ms) {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, "budget flush: could not serialize usage events; will retry");
            runtime.keys.rollback_flush(batch);
            return;
        }
    };
    match runtime.store.persist_flush(&batch.rows(), &events).await {
        Ok(()) => runtime.keys.commit_flush(batch),
        Err(error) => {
            tracing::warn!(%error, "budget flush failed; will retry next interval");
            runtime.keys.rollback_flush(batch);
        }
    }
}
