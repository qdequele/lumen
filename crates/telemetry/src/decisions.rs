//! Decisions counters (ADR 017): deprecated-route traffic and refusals.

use crate::metrics::Metrics;
use prometheus::{IntCounterVec, Opts};

/// Counters for the decisions capability. Cheap to clone (the inner
/// counters are `Arc`-backed).
#[derive(Debug, Clone)]
pub struct DecisionMetrics {
    deprecated_requests_total: IntCounterVec,
    decision_refusals_total: IntCounterVec,
}

impl DecisionMetrics {
    fn counters() -> Result<Self, prometheus::Error> {
        Ok(Self {
            deprecated_requests_total: IntCounterVec::new(
                Opts::new(
                    "lumen_deprecated_requests_total",
                    "Requests to a deprecated route.",
                ),
                &["route"],
            )?,
            decision_refusals_total: IntCounterVec::new(
                Opts::new(
                    "lumen_decision_refusals_total",
                    "Decision questions the upstream refused.",
                ),
                &["model"],
            )?,
        })
    }

    /// Register `lumen_deprecated_requests_total{route}` and
    /// `lumen_decision_refusals_total{model}`.
    ///
    /// # Errors
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let m = Self::counters()?;
        let registry = metrics.registry();
        registry.register(Box::new(m.deprecated_requests_total.clone()))?;
        registry.register(Box::new(m.decision_refusals_total.clone()))?;
        Ok(m)
    }

    /// Counters registered nowhere (a second `AppState` on the same
    /// registry); they count but are not exported.
    #[must_use]
    pub fn detached() -> Self {
        // Justified exception to the no-expect rule: the names, help texts
        // and labels are static and valid, so construction cannot fail (the
        // unit test below exercises this exact path).
        #[allow(clippy::expect_used)]
        Self::counters().expect("static metric definitions are valid")
    }

    /// Count one request to a deprecated route.
    pub fn inc_deprecated(&self, route: &str) {
        self.deprecated_requests_total
            .with_label_values(&[route])
            .inc();
    }

    /// Count `n` refused questions of `model`.
    pub fn add_refusals(&self, model: &str, n: u64) {
        if n > 0 {
            self.decision_refusals_total
                .with_label_values(&[model])
                .inc_by(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_register_and_export() {
        let metrics = Metrics::new();
        let d = DecisionMetrics::register(&metrics).unwrap();
        d.inc_deprecated("/v1/systemone");
        d.add_refusals("luna", 2);
        d.add_refusals("jev", 0);
        let text = metrics.encode_text();
        assert!(text.contains(r#"lumen_deprecated_requests_total{route="/v1/systemone"} 1"#));
        assert!(text.contains(r#"lumen_decision_refusals_total{model="luna"} 2"#));
        assert!(
            !text.contains(r#"model="jev""#),
            "zero refusals create no series"
        );
        assert!(DecisionMetrics::register(&metrics).is_err());
    }

    #[test]
    fn detached_counters_count_without_a_registry() {
        let d = DecisionMetrics::detached();
        d.inc_deprecated("/v1/systemone");
        assert_eq!(
            d.deprecated_requests_total
                .with_label_values(&["/v1/systemone"])
                .get(),
            1
        );
    }
}
