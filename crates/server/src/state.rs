//! Shared application state handed to axum handlers.

use ferrogate_telemetry::Metrics;

/// Cheap-to-clone state shared across all handlers.
///
/// Holds only process-wide, secret-free handles (currently the metrics
/// registry). It is `Clone` because axum clones state per request.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The Prometheus metrics registry.
    pub metrics: Metrics,
}

impl AppState {
    /// Create fresh application state with an empty metrics registry.
    #[must_use]
    pub fn new(metrics: Metrics) -> Self {
        Self { metrics }
    }
}
