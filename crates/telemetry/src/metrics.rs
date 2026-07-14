//! The Prometheus registry and text encoding.

use prometheus::{Encoder, Registry, TextEncoder};

/// Holds the process-wide Prometheus [`Registry`].
///
/// Cloning is cheap - the inner registry is reference-counted - so `Metrics`
/// can be stored in axum state and shared across handlers.
#[derive(Debug, Clone)]
pub struct Metrics {
    registry: Registry,
}

impl Metrics {
    /// Create a fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Registry::new(),
        }
    }

    /// Borrow the registry so collectors can be registered against it.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Encode all registered metrics in the Prometheus text exposition format.
    ///
    /// Returns an empty body when no collectors are registered (M1 baseline),
    /// which is a valid `/metrics` response.
    #[must_use]
    pub fn encode_text(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        // Encoding into an in-memory `Vec<u8>` cannot fail for well-formed
        // metric families; fall back to an empty body rather than panicking.
        if encoder.encode(&metric_families, &mut buffer).is_err() {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    /// The Prometheus text exposition content type, for the `/metrics` header.
    ///
    /// Derived from the encoder itself so it can never drift from what
    /// [`encode_text`](Self::encode_text) actually produces.
    #[must_use]
    pub fn content_type(&self) -> String {
        TextEncoder::new().format_type().to_owned()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_encodes_to_empty_body() {
        let m = Metrics::new();
        assert!(m.encode_text().is_empty());
    }

    #[test]
    fn registered_counter_appears_in_output() {
        let m = Metrics::new();
        let counter = prometheus::IntCounter::new("fg_test_total", "test counter").unwrap();
        m.registry().register(Box::new(counter.clone())).unwrap();
        counter.inc();
        let out = m.encode_text();
        assert!(out.contains("fg_test_total"));
        assert!(out.contains('1'));
    }
}
