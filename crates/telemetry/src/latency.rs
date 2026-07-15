//! Request-latency histograms.
//!
//! Two layers, because "how long did it take" means two different things:
//!
//! * [`observe_http`](LatencyMetrics::observe_http) - wall time of EVERY HTTP
//!   request through the gateway (including `/health`, `/metrics` and admin
//!   routes), labelled by method, matched route and status. For streaming
//!   responses this measures time-to-response-headers: the body is handed to
//!   the client as a stream, so the middleware cannot (and should not) wait
//!   for it.
//! * [`observe_request`](LatencyMetrics::observe_request) - end-to-end time of
//!   one accounted API call (chat/embed/rerank), labelled by capability, model
//!   and provider. For streaming chat this covers the FULL stream, because it
//!   is recorded when accounting closes (stream end or client disconnect).
//!
//! Cardinality is bounded by construction: `path` is the axum route template
//! (never the raw URI), models/providers are operator-configured, and status
//! is an HTTP status code. Recording is a lock-free atomic observe - safe on
//! the hot path.

use crate::metrics::Metrics;
use prometheus::{HistogramOpts, HistogramVec, Opts};

/// Histogram buckets, in seconds. The gateway spans four orders of magnitude:
/// sub-millisecond operational endpoints up to multi-minute LLM completions.
const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Latency histograms, registered against one [`Metrics`] registry. Cheap to
/// clone (the inner vectors are `Arc`-backed).
#[derive(Debug, Clone)]
pub struct LatencyMetrics {
    http_duration: HistogramVec,
    request_duration: HistogramVec,
}

impl LatencyMetrics {
    /// Register `lumen_http_request_duration_seconds{method,path,status}` and
    /// `lumen_request_duration_seconds{capability,model,provider,status}`.
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] if a collector is registered twice.
    pub fn register(metrics: &Metrics) -> Result<Self, prometheus::Error> {
        let http_duration = HistogramVec::new(
            HistogramOpts {
                common_opts: Opts::new(
                    "lumen_http_request_duration_seconds",
                    "Wall time of every HTTP request, by method, matched route and status. \
                     Streaming responses count time-to-response-headers.",
                ),
                buckets: LATENCY_BUCKETS.to_vec(),
            },
            &["method", "path", "status"],
        )?;
        let request_duration = HistogramVec::new(
            HistogramOpts {
                common_opts: Opts::new(
                    "lumen_request_duration_seconds",
                    "End-to-end latency of accounted API calls, by capability, served \
                     model/provider and status. Streaming chat covers the full stream.",
                ),
                buckets: LATENCY_BUCKETS.to_vec(),
            },
            &["capability", "model", "provider", "status"],
        )?;
        let registry = metrics.registry();
        registry.register(Box::new(http_duration.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        Ok(Self {
            http_duration,
            request_duration,
        })
    }

    /// Record one HTTP request. `path` must be the matched route template
    /// (or a fixed sentinel such as `"unmatched"`), never the raw URI.
    pub fn observe_http(&self, method: &str, path: &str, status: u16, seconds: f64) {
        self.http_duration
            .with_label_values(&[method, path, status_str(status)])
            .observe(seconds);
    }

    /// Record one accounted API call (chat/embed/rerank), attributed to the
    /// model/provider that actually served it.
    pub fn observe_request(
        &self,
        capability: &str,
        model: &str,
        provider: &str,
        status: u16,
        seconds: f64,
    ) {
        self.request_duration
            .with_label_values(&[capability, model, provider, status_str(status)])
            .observe(seconds);
    }
}

/// Static strings for the status codes the gateway actually returns, so the
/// hot path allocates nothing. Anything unexpected degrades to a coarse class
/// label rather than allocating or panicking.
const fn status_str(status: u16) -> &'static str {
    match status {
        200 => "200",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        408 => "408",
        409 => "409",
        413 => "413",
        422 => "422",
        429 => "429",
        // LM-6001 client-cancellation (issue #11): its own label, distinct
        // from both the generic "4xx" class and (crucially) "500"/"5xx" - a
        // client hanging up must never be counted against internal-error
        // rates or alerts.
        499 => "499",
        500 => "500",
        502 => "502",
        503 => "503",
        504 => "504",
        100..=199 => "1xx",
        201..=299 => "2xx",
        300..=399 => "3xx",
        _ if status <= 499 => "4xx",
        _ => "5xx",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Metrics, LatencyMetrics) {
        let metrics = Metrics::new();
        let latency = LatencyMetrics::register(&metrics).expect("register");
        (metrics, latency)
    }

    #[test]
    fn http_observation_appears_with_all_labels() {
        let (metrics, latency) = setup();
        latency.observe_http("GET", "/health", 200, 0.0004);
        let out = metrics.encode_text();
        assert!(out.contains("lumen_http_request_duration_seconds_bucket"));
        assert!(out.contains(r#"method="GET""#));
        assert!(out.contains(r#"path="/health""#));
        assert!(out.contains(r#"status="200""#));
        assert!(out.contains("lumen_http_request_duration_seconds_count"));
    }

    #[test]
    fn request_observation_appears_with_all_labels() {
        let (metrics, latency) = setup();
        latency.observe_request("chat", "gpt", "openai", 200, 1.5);
        let out = metrics.encode_text();
        assert!(out.contains("lumen_request_duration_seconds_bucket"));
        assert!(out.contains(r#"capability="chat""#));
        assert!(out.contains(r#"model="gpt""#));
        assert!(out.contains(r#"provider="openai""#));
    }

    #[test]
    fn sum_reflects_the_observed_seconds() {
        let (metrics, latency) = setup();
        latency.observe_http("GET", "/health", 200, 0.25);
        latency.observe_http("GET", "/health", 200, 0.75);
        let out = metrics.encode_text();
        let sum_line = out
            .lines()
            .find(|l| l.starts_with("lumen_http_request_duration_seconds_sum"))
            .expect("sum line");
        let value: f64 = sum_line
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse().ok())
            .expect("numeric sum");
        assert!((value - 1.0).abs() < f64::EPSILON, "{sum_line}");
    }

    #[test]
    fn uncommon_status_degrades_to_a_class_label() {
        assert_eq!(status_str(200), "200");
        assert_eq!(status_str(502), "502");
        assert_eq!(status_str(418), "4xx");
        assert_eq!(status_str(599), "5xx");
        assert_eq!(status_str(302), "3xx");
        assert_eq!(status_str(101), "1xx");
    }

    // Issue #11: the client-cancellation status (LM-6001, HTTP 499) gets its
    // own label rather than being folded into the generic "4xx" class, so it
    // is distinguishable in `/metrics` from both real client errors and the
    // `5xx`/`500` internal-error bucket it used to inflate.
    #[test]
    fn client_cancelled_status_gets_its_own_label_not_5xx() {
        assert_eq!(status_str(499), "499");
        assert_ne!(status_str(499), "5xx");
    }

    #[test]
    fn double_registration_is_an_error() {
        let (metrics, _latency) = setup();
        assert!(LatencyMetrics::register(&metrics).is_err());
    }

    #[test]
    fn buckets_cover_sub_millisecond_and_multi_minute() {
        let first = LATENCY_BUCKETS.first().copied().unwrap_or(0.0);
        let last = LATENCY_BUCKETS.last().copied().unwrap_or(0.0);
        assert!(
            first <= 0.001,
            "need sub-ms resolution for the ops endpoints"
        );
        assert!(last >= 60.0, "need multi-minute reach for LLM completions");
    }
}
