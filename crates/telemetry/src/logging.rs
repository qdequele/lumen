//! Structured logging setup.
//!
//! Prompts and request bodies are NEVER logged: the gateway treats user data
//! as radioactive. Only metadata (request id, model, status, latency) is
//! emitted, and that is the responsibility of the request span, not this
//! module - which only wires up the subscriber.

use tracing_subscriber::EnvFilter;

/// Output format for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, coloured output for local development.
    Pretty,
    /// One JSON object per line, for production log pipelines.
    Json,
}

/// Initialise the global tracing subscriber.
///
/// Reads `RUST_LOG` for filtering, falling back to `default_directive`
/// (e.g. `"info"`). Safe to call more than once: a second call is a no-op
/// (returns `false`) rather than a panic, which keeps tests independent.
pub fn init_logging(format: LogFormat, default_directive: &str) -> bool {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    match format {
        LogFormat::Pretty => builder.try_init().is_ok(),
        LogFormat::Json => builder.json().flatten_event(true).try_init().is_ok(),
    }
}
