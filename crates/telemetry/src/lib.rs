//! Prometheus metrics and structured logging for Ferrogate.
//!
//! Metrics live in a single [`prometheus::Registry`] wrapped by [`Metrics`].
//! In M1 the registry is empty; request/provider metrics are registered in
//! later milestones. Logging is initialised once at boot via [`init_logging`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod logging;
pub mod metrics;

pub use logging::init_logging;
pub use metrics::Metrics;
