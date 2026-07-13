//! Resilience gauges (M6): circuit-breaker state and background provider health.
//!
//! Both are low-cardinality: `circuit_state` is bounded by the configured
//! (provider × model) pairs and `provider_up` by the provider count — all
//! operator-defined. The numeric encodings are deliberately decoupled from the
//! router's own enums (the router maps its state to the numbers) so this crate
//! never has to depend on `router`.

use crate::metrics::Metrics;
use prometheus::{IntGaugeVec, Opts};

/// Circuit-breaker state, as exported on `ferrogate_circuit_state`.
pub const CIRCUIT_CLOSED: i64 = 0;
/// The breaker is open — calls are short-circuited.
pub const CIRCUIT_OPEN: i64 = 1;
/// The breaker is half-open — a single probe is allowed through.
pub const CIRCUIT_HALF_OPEN: i64 = 2;

/// M6 gauges, registered against one [`Metrics`] registry. Cheap to clone
/// (the inner gauges are `Arc`-backed), so it can be shared across the router's
/// circuit breakers and the server's health-check task.
#[derive(Debug, Clone)]
pub struct ResilienceMetrics {
    circuit_state: IntGaugeVec,
    provider_up: IntGaugeVec,
}

impl ResilienceMetrics {
    /// Register `ferrogate_circuit_state{provider,model}` and
    /// `ferrogate_provider_up{provider}`.
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let circuit_state = IntGaugeVec::new(
            Opts::new(
                "ferrogate_circuit_state",
                "Circuit-breaker state per (provider, model): 0 closed, 1 open, 2 half-open.",
            ),
            &["provider", "model"],
        )?;
        let provider_up = IntGaugeVec::new(
            Opts::new(
                "ferrogate_provider_up",
                "Background health probe result per provider: 1 up, 0 down (absent = unknown).",
            ),
            &["provider"],
        )?;
        let registry = metrics.registry();
        registry.register(Box::new(circuit_state.clone()))?;
        registry.register(Box::new(provider_up.clone()))?;
        Ok(Self {
            circuit_state,
            provider_up,
        })
    }

    /// Set the circuit-breaker gauge for one (provider, model). `state` is one
    /// of the `CIRCUIT_*` constants.
    pub fn set_circuit_state(&self, provider: &str, model: &str, state: i64) {
        self.circuit_state
            .with_label_values(&[provider, model])
            .set(state);
    }

    /// Record a background health-probe result for one provider.
    pub fn set_provider_up(&self, provider: &str, up: bool) {
        self.provider_up
            .with_label_values(&[provider])
            .set(i64::from(up));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_state_gauge_reports_the_latest_value() {
        let metrics = Metrics::new();
        let r = ResilienceMetrics::register(&metrics).unwrap();
        r.set_circuit_state("openai", "gpt-4o", CIRCUIT_OPEN);
        let out = metrics.encode_text();
        assert!(out.contains("ferrogate_circuit_state"));
        assert!(out.contains(r#"provider="openai""#));
        assert!(out.contains(r#"model="gpt-4o""#));
        // Gauge is last-write-wins.
        r.set_circuit_state("openai", "gpt-4o", CIRCUIT_CLOSED);
        let out = metrics.encode_text();
        assert!(out.contains("ferrogate_circuit_state{model=\"gpt-4o\",provider=\"openai\"} 0"));
    }

    #[test]
    fn provider_up_gauge_encodes_bool_as_1_or_0() {
        let metrics = Metrics::new();
        let r = ResilienceMetrics::register(&metrics).unwrap();
        r.set_provider_up("tei", true);
        r.set_provider_up("ollama", false);
        let out = metrics.encode_text();
        assert!(out.contains("ferrogate_provider_up{provider=\"tei\"} 1"));
        assert!(out.contains("ferrogate_provider_up{provider=\"ollama\"} 0"));
    }
}
