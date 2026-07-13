//! Shared application state handed to axum handlers.

use crate::auth::AuthRuntime;
use crate::pricing::CostTable;
use crate::resilience::ResilienceRuntime;
use ferrogate_auth::usage::UsageLogger;
use ferrogate_providers::Registry;
use ferrogate_telemetry::{Metrics, TokenMetrics};
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
/// Holds only process-wide, secret-free handles; the heavy pieces sit behind
/// `Arc`. It is `Clone` because axum clones state per request.
#[derive(Clone)]
pub struct AppState {
    /// The Prometheus metrics registry.
    pub metrics: Metrics,
    /// The provider routing table.
    pub registry: Arc<Registry>,
    /// Chat streaming guard timings.
    pub guards: StreamGuards,
    /// Token-accounting counters (ADR 003) — always on.
    pub tokens: TokenMetrics,
    /// Virtual-key auth runtime; `None` = auth disabled (open gateway).
    pub auth: Option<Arc<AuthRuntime>>,
    /// Usage-log channel; `None` = no usage database.
    pub usage: Option<UsageLogger>,
    /// Per-model price table (M5 cost counting).
    pub pricing: Arc<CostTable>,
    /// Resilience runtime: circuit breakers, retry policy, timeouts, fallback
    /// chains (M6). All in-memory; never on a database path.
    pub resilience: Arc<ResilienceRuntime>,
}

impl AppState {
    /// Create application state. `tokens` must be registered against
    /// `metrics` (see [`TokenMetrics::register`]); auth, usage logging and
    /// pricing are attached with the builder methods below.
    #[must_use]
    pub fn new(metrics: Metrics, registry: Arc<Registry>, tokens: TokenMetrics) -> Self {
        Self {
            metrics,
            registry,
            guards: StreamGuards::default(),
            tokens,
            auth: None,
            usage: None,
            pricing: Arc::new(CostTable::default()),
            resilience: Arc::new(ResilienceRuntime::defaults()),
        }
    }

    /// Attach the resilience runtime (builder style).
    #[must_use]
    pub fn with_resilience(mut self, resilience: Arc<ResilienceRuntime>) -> Self {
        self.resilience = resilience;
        self
    }

    /// Replace the stream guard timings (builder style).
    #[must_use]
    pub fn with_guards(mut self, guards: StreamGuards) -> Self {
        self.guards = guards;
        self
    }

    /// Attach the virtual-key auth runtime (builder style).
    #[must_use]
    pub fn with_auth(mut self, auth: Arc<AuthRuntime>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Attach the usage-log channel (builder style).
    #[must_use]
    pub fn with_usage(mut self, usage: UsageLogger) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Attach the per-model price table (builder style).
    #[must_use]
    pub fn with_pricing(mut self, pricing: CostTable) -> Self {
        self.pricing = Arc::new(pricing);
        self
    }
}
