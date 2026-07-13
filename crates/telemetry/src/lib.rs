//! Prometheus metrics and structured logging for Ferrogate.
//!
//! Metrics live in a single [`prometheus::Registry`] wrapped by [`Metrics`].
//! Token accounting (ADR 003) is registered via [`TokenMetrics`]. Logging is
//! initialised once at boot via [`init_logging`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod logging;
pub mod metrics;
pub mod tokens;

pub use logging::init_logging;
pub use metrics::Metrics;
pub use tokens::{Direction, TokenMetrics};
