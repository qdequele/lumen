//! Billing usage-event delivery metrics (ADR 015). No payload detail: no key
//! ids, no amounts.

use crate::metrics::Metrics;
use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts};

/// Outbox and sender metrics. Cheap to clone.
#[derive(Debug, Clone)]
pub struct UsageEventMetrics {
    pending: IntGauge,
    oldest_pending_seconds: IntGauge,
    delivered_total: IntCounter,
    failed_total: IntCounterVec,
}

impl UsageEventMetrics {
    /// Register the collectors.
    ///
    /// # Errors
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let pending = IntGauge::new(
            "lumen_usage_events_pending",
            "Billing usage events written to the outbox and not yet acknowledged.",
        )?;
        let oldest_pending_seconds = IntGauge::new(
            "lumen_usage_events_oldest_pending_seconds",
            "Age of the oldest unacknowledged billing usage event, in seconds (0 when none).",
        )?;
        let delivered_total = IntCounter::new(
            "lumen_usage_events_delivered_total",
            "Billing usage events acknowledged by the control plane.",
        )?;
        let failed_total = IntCounterVec::new(
            Opts::new(
                "lumen_usage_events_failed_total",
                "Billing usage events whose delivery attempt failed, by reason; they stay pending.",
            ),
            &["reason"],
        )?;
        let registry = metrics.registry();
        registry.register(Box::new(pending.clone()))?;
        registry.register(Box::new(oldest_pending_seconds.clone()))?;
        registry.register(Box::new(delivered_total.clone()))?;
        registry.register(Box::new(failed_total.clone()))?;
        Ok(Self {
            pending,
            oldest_pending_seconds,
            delivered_total,
            failed_total,
        })
    }

    /// Current pending count.
    pub fn set_pending(&self, n: i64) {
        self.pending.set(n);
    }

    /// Current oldest pending age.
    pub fn set_oldest_pending_seconds(&self, secs: i64) {
        self.oldest_pending_seconds.set(secs);
    }

    /// `n` events acknowledged.
    pub fn add_delivered(&self, n: u64) {
        self.delivered_total.inc_by(n);
    }

    /// Events that failed for `reason` (`connect`, `timeout`, `auth`,
    /// `status`, `malformed`, `not_accepted`, `store`), one increment per event.
    pub fn inc_failed(&self, reason: &str, n: u64) {
        self.failed_total.with_label_values(&[reason]).inc_by(n);
    }
}
