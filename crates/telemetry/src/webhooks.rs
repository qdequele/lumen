//! Outbound budget-webhook counters (ADR 011).
//!
//! Four counters and one histogram tell an operator whether the billing
//! integration is actually working: how many events were queued, how many the
//! bounded queue had to drop, how many died after exhausting their retry
//! budget, and how long deliveries take. Never any payload detail - the
//! metrics carry no key ids and no budget figures.

use crate::metrics::Metrics;
use prometheus::{Histogram, HistogramOpts, IntCounter};

/// Counters for the webhook queue and sender. Cheap to clone (the inner
/// collectors are `Arc`-backed), so one handle can sit on the request path's
/// queue and another in the sender task.
#[derive(Debug, Clone)]
pub struct WebhookMetrics {
    queued_total: IntCounter,
    sent_total: IntCounter,
    dropped_total: IntCounter,
    retries_total: IntCounter,
    dead_total: IntCounter,
    delivery_seconds: Histogram,
}

impl WebhookMetrics {
    /// Register the webhook collectors.
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let queued_total = IntCounter::new(
            "lumen_webhook_queued_total",
            "Webhook events accepted into the bounded queue, across every configured event kind.",
        )?;
        let sent_total = IntCounter::new(
            "lumen_webhook_sent_total",
            "Webhook events the receiver acknowledged with a 2xx.",
        )?;
        let dropped_total = IntCounter::new(
            "lumen_webhook_dropped_total",
            "Webhook events dropped because the queue was full; the usage export route remains the system of record (ADR 011).",
        )?;
        let retries_total = IntCounter::new(
            "lumen_webhook_retries_total",
            "Webhook delivery attempts that failed and were retried with backoff.",
        )?;
        let dead_total = IntCounter::new(
            "lumen_webhook_dead_total",
            "Webhook events abandoned after exhausting their retry budget or hitting a permanent receiver error.",
        )?;
        let delivery_seconds = Histogram::with_opts(HistogramOpts::new(
            "lumen_webhook_delivery_seconds",
            "Wall time of a single webhook delivery attempt, in seconds.",
        ))?;

        let registry = metrics.registry();
        registry.register(Box::new(queued_total.clone()))?;
        registry.register(Box::new(sent_total.clone()))?;
        registry.register(Box::new(dropped_total.clone()))?;
        registry.register(Box::new(retries_total.clone()))?;
        registry.register(Box::new(dead_total.clone()))?;
        registry.register(Box::new(delivery_seconds.clone()))?;

        Ok(Self {
            queued_total,
            sent_total,
            dropped_total,
            retries_total,
            dead_total,
            delivery_seconds,
        })
    }

    /// One event accepted into the queue.
    pub fn inc_queued(&self) {
        self.queued_total.inc();
    }

    /// One event dropped by a full queue.
    pub fn inc_dropped(&self) {
        self.dropped_total.inc();
    }

    /// One event acknowledged by the receiver.
    pub fn inc_sent(&self) {
        self.sent_total.inc();
    }

    /// One failed attempt that will be retried.
    pub fn inc_retry(&self) {
        self.retries_total.inc();
    }

    /// One event abandoned (retries exhausted, or a permanent rejection).
    pub fn inc_dead(&self) {
        self.dead_total.inc();
    }

    /// Record the duration of one delivery attempt.
    pub fn observe_delivery(&self, seconds: f64) {
        self.delivery_seconds.observe(seconds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_counters_register_and_increment() {
        let metrics = Metrics::new();
        let w = WebhookMetrics::register(&metrics).expect("registers");
        w.inc_queued();
        w.inc_sent();
        w.inc_dropped();
        w.inc_dropped();
        w.inc_retry();
        w.inc_dead();
        w.observe_delivery(0.05);

        let out = metrics.encode_text();
        assert!(out.contains("lumen_webhook_queued_total 1"));
        assert!(out.contains("lumen_webhook_sent_total 1"));
        assert!(out.contains("lumen_webhook_dropped_total 2"));
        assert!(out.contains("lumen_webhook_retries_total 1"));
        assert!(out.contains("lumen_webhook_dead_total 1"));
        assert!(out.contains("lumen_webhook_delivery_seconds_count 1"));
    }

    #[test]
    fn registering_twice_against_one_registry_is_an_error() {
        let metrics = Metrics::new();
        WebhookMetrics::register(&metrics).expect("first registration");
        assert!(WebhookMetrics::register(&metrics).is_err());
    }
}
