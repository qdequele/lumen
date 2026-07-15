//! Shared application state handed to axum handlers.

use crate::auth::AuthRuntime;
use crate::health::ProviderHealth;
use crate::pricing::CostTable;
use crate::resilience::ResilienceRuntime;
use arc_swap::ArcSwap;
use lumen_auth::usage::UsageLogger;
use lumen_providers::image_fetch::ImageFetchPolicy;
use lumen_providers::Registry;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use std::sync::Arc;
use std::time::Duration;

/// Guard timings for the chat request path (see `chat::to_event_stream`).
///
/// Fine-grained per-phase upstream timeouts (connect / total) arrive in M6;
/// these two cover the M4 acceptance criteria.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGuards {
    /// How long to wait for the upstream's first sign of life before failing
    /// with LM-3011 (504). Streaming: the first SSE frame; non-streaming: the
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
    /// Token-accounting counters (ADR 003) - always on.
    pub tokens: TokenMetrics,
    /// Request-latency histograms (HTTP + per-capability) - always on.
    pub latency: LatencyMetrics,
    /// Virtual-key auth runtime; `None` = auth disabled (open gateway).
    pub auth: Option<Arc<AuthRuntime>>,
    /// Usage-log channel; `None` = no usage database.
    pub usage: Option<UsageLogger>,
    /// Per-model price table (M5 cost counting), behind an [`ArcSwap`] so a
    /// config hot reload can replace it atomically (DEBT-1). Read a consistent
    /// per-request snapshot via [`pricing`](AppState::pricing).
    pub pricing: Arc<ArcSwap<CostTable>>,
    /// Resilience runtime: circuit breakers, retry policy, timeouts, fallback
    /// chains (M6). All in-memory; never on a database path.
    pub resilience: Arc<ResilienceRuntime>,
    /// Background provider-health registry (M6 §6.5), for `/health/providers`.
    pub health: Arc<ProviderHealth>,
    /// Configured max request body size in bytes (for the `LM-1002` message).
    pub body_limit: usize,
    /// Guarded image-fetch policy for multimodal embeddings (M9). Default:
    /// disabled (a remote image URL yields `LM-2005`).
    pub image_fetch: Arc<ImageFetchPolicy>,
    /// Hot-reload trigger; `Some` when the config reloader is armed. The admin
    /// API pings it after storing a provider key so the rotation is applied
    /// without a restart (the reloader re-reads the key from the DB). `None` =
    /// no reloader (e.g. tests, or a watcher-setup failure at boot).
    pub reload_trigger: Option<Arc<tokio::sync::Notify>>,
}

impl AppState {
    /// Create application state. `tokens` and `latency` must be registered
    /// against `metrics` (see [`TokenMetrics::register`] and
    /// [`LatencyMetrics::register`]); auth, usage logging and pricing are
    /// attached with the builder methods below.
    #[must_use]
    pub fn new(
        metrics: Metrics,
        registry: Arc<Registry>,
        tokens: TokenMetrics,
        latency: LatencyMetrics,
    ) -> Self {
        Self {
            metrics,
            registry,
            guards: StreamGuards::default(),
            tokens,
            latency,
            auth: None,
            usage: None,
            pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
            resilience: Arc::new(ResilienceRuntime::defaults()),
            health: Arc::new(ProviderHealth::default()),
            // Matches `config::default_body_limit()`; overridden via
            // `with_body_limit` once the real config is known (main.rs boot).
            body_limit: 10 * 1024 * 1024,
            image_fetch: Arc::new(ImageFetchPolicy::default()),
            reload_trigger: None,
        }
    }

    /// Attach the guarded image-fetch policy (builder style).
    #[must_use]
    pub fn with_image_fetch(mut self, policy: Arc<ImageFetchPolicy>) -> Self {
        self.image_fetch = policy;
        self
    }

    /// Attach the hot-reload trigger (builder style) so the admin API can
    /// request a reload after a provider-key rotation.
    #[must_use]
    pub fn with_reload_trigger(mut self, trigger: Arc<tokio::sync::Notify>) -> Self {
        self.reload_trigger = Some(trigger);
        self
    }

    /// Attach the resilience runtime (builder style).
    #[must_use]
    pub fn with_resilience(mut self, resilience: Arc<ResilienceRuntime>) -> Self {
        self.resilience = resilience;
        self
    }

    /// Attach the provider-health registry (builder style).
    #[must_use]
    pub fn with_health(mut self, health: Arc<ProviderHealth>) -> Self {
        self.health = health;
        self
    }

    /// Set the request body-size limit surfaced in `LM-1002`.
    #[must_use]
    pub fn with_body_limit(mut self, body_limit: usize) -> Self {
        self.body_limit = body_limit;
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
        self.pricing = Arc::new(ArcSwap::from_pointee(pricing));
        self
    }

    /// Attach a shared price-table cell (builder style) - used when the hot
    /// reloader must swap the same cell the handlers read (DEBT-1).
    #[must_use]
    pub fn with_pricing_cell(mut self, pricing: Arc<ArcSwap<CostTable>>) -> Self {
        self.pricing = pricing;
        self
    }

    /// A consistent per-request snapshot of the price table. Taken once per
    /// request so a mid-request hot reload can't change prices between the
    /// pre-call estimate and the post-call settlement.
    #[must_use]
    pub fn pricing(&self) -> Arc<CostTable> {
        self.pricing.load_full()
    }
}
