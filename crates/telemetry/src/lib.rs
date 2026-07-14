//! Prometheus metrics and structured logging for LUMEN.
//!
//! Metrics live in a single [`prometheus::Registry`] wrapped by [`Metrics`].
//! Token accounting (ADR 003) is registered via [`TokenMetrics`]. Logging is
//! initialised once at boot via [`init_logging`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod latency;
pub mod logging;
pub mod metrics;
pub mod reload;
pub mod resilience;
pub mod tokens;

pub use latency::LatencyMetrics;
pub use logging::init_logging;
pub use metrics::Metrics;
pub use reload::ReloadMetrics;
pub use resilience::ResilienceMetrics;
pub use tokens::{Direction, TokenMetrics};
