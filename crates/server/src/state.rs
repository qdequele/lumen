//! Shared application state handed to axum handlers.

use ferrogate_providers::Registry;
use ferrogate_telemetry::Metrics;
use std::sync::Arc;
use std::time::Duration;

/// Guard timings for the chat request path (see `chat::to_event_stream`).
///
/// Fine-grained per-phase upstream timeouts (connect / total) arrive in M6;
/// these two cover the M4 acceptance criteria.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGuards {
    /// How long to wait for the upstream's first sign of life before failing
    /// with FG-3011 (504). Streaming: the first SSE frame; non-streaming: the
    /// whole upstream call (indivisible until per-phase timeouts land in M6).
    pub first_token_timeout: Duration,
    /// Idle interval after which a `: ping` SSE comment keeps intermediaries
    /// from reaping a silent stream.
    pub heartbeat_interval: Duration,
}

impl Default for StreamGuards {
    fn default() -> Self {
        Self {
            first_token_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

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
    /// Chat streaming guard timings.
    pub guards: StreamGuards,
}

impl AppState {
    /// Create application state from the metrics and provider registries,
    /// with default [`StreamGuards`].
    #[must_use]
    pub fn new(metrics: Metrics, registry: Arc<Registry>) -> Self {
        Self {
            metrics,
            registry,
            guards: StreamGuards::default(),
        }
    }

    /// Replace the stream guard timings (builder style).
    #[must_use]
    pub fn with_guards(mut self, guards: StreamGuards) -> Self {
        self.guards = guards;
        self
    }
}
