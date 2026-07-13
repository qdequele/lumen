//! Config hot-reload counters (M7 §7.3).

use crate::metrics::Metrics;
use prometheus::IntCounter;

/// Counters for configuration hot reloads. Cheap to clone (the inner counters
/// are `Arc`-backed), so a handle can move into the reload task.
#[derive(Debug, Clone)]
pub struct ReloadMetrics {
    reloads_total: IntCounter,
    reload_failures_total: IntCounter,
}

impl ReloadMetrics {
    /// Register `ferrogate_config_reloads_total` and
    /// `ferrogate_config_reload_failures_total`.
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let reloads_total = IntCounter::new(
            "ferrogate_config_reloads_total",
            "Successful configuration hot reloads (atomic registry swaps).",
        )?;
        let reload_failures_total = IntCounter::new(
            "ferrogate_config_reload_failures_total",
            "Configuration reloads rejected as invalid; the previous config was kept.",
        )?;
        let registry = metrics.registry();
        registry.register(Box::new(reloads_total.clone()))?;
        registry.register(Box::new(reload_failures_total.clone()))?;
        Ok(Self {
            reloads_total,
            reload_failures_total,
        })
    }

    /// Record a successful reload + swap.
    pub fn inc_success(&self) {
        self.reloads_total.inc();
    }

    /// Record a rejected reload (invalid config; old config kept).
    pub fn inc_failure(&self) {
        self.reload_failures_total.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_counters_register_and_increment() {
        let metrics = Metrics::new();
        let r = ReloadMetrics::register(&metrics).unwrap();
        r.inc_success();
        r.inc_failure();
        r.inc_failure();
        let out = metrics.encode_text();
        assert!(out.contains("ferrogate_config_reloads_total 1"));
        assert!(out.contains("ferrogate_config_reload_failures_total 2"));
    }
}
