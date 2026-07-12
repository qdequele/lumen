//! Shared application state handed to axum handlers.

use ferrogate_providers::Registry;
use ferrogate_telemetry::Metrics;
use std::sync::Arc;

/// Cheap-to-clone state shared across all handlers.
///
/// Holds only process-wide, secret-free handles (the metrics registry and the
/// provider registry). It is `Clone` because axum clones state per request; the
/// heavy pieces sit behind `Arc`.
#[derive(Clone)]
pub struct AppState {
    /// The Prometheus metrics registry.
    pub metrics: Metrics,
    /// The provider routing table.
    pub registry: Arc<Registry>,
}

impl AppState {
    /// Create application state from the metrics and provider registries.
    #[must_use]
    pub fn new(metrics: Metrics, registry: Arc<Registry>) -> Self {
        Self { metrics, registry }
    }
}
