//! Token-accounting Prometheus counters (ADR 003) and the ADR 002
//! metadata-label plumbing.
//!
//! Cardinality is fixed by construction: the base labels are enums or
//! operator-configured model/provider names, and metadata labels exist only
//! for keys the operator explicitly allowlisted (`telemetry.metadata_labels`,
//! default empty) — never for arbitrary client-supplied keys.

use crate::metrics::Metrics;
use prometheus::{IntCounter, IntCounterVec, Opts};

/// Base label names of `lumen_tokens_total`.
const TOKEN_LABELS: [&str; 5] = ["capability", "model", "provider", "direction", "estimated"];
/// Base label names of `lumen_rerank_search_units_total`.
const SEARCH_UNIT_LABELS: [&str; 2] = ["model", "provider"];

/// All M5 token/usage counters, registered against one [`Metrics`] registry.
#[derive(Debug, Clone)]
pub struct TokenMetrics {
    tokens_total: IntCounterVec,
    rerank_search_units_total: IntCounterVec,
    tokens_estimated_total: IntCounter,
    usage_log_dropped_total: IntCounter,
    metadata_rejected_total: IntCounter,
    /// Allowlisted metadata keys, in registration order. Values passed to the
    /// record methods must align with this order.
    metadata_labels: Vec<String>,
}

impl TokenMetrics {
    /// Register the counters. `metadata_labels` is the operator allowlist
    /// (ADR 002); each entry becomes an extra label on the token counters.
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] when a metadata label is not a valid Prometheus
    /// label name or a counter is registered twice.
    pub fn register(
        metrics: &Metrics,
        metadata_labels: &[String],
    ) -> Result<Self, prometheus::Error> {
        let token_label_names: Vec<&str> = TOKEN_LABELS
            .iter()
            .copied()
            .chain(metadata_labels.iter().map(String::as_str))
            .collect();
        let unit_label_names: Vec<&str> = SEARCH_UNIT_LABELS
            .iter()
            .copied()
            .chain(metadata_labels.iter().map(String::as_str))
            .collect();

        let tokens_total = IntCounterVec::new(
            Opts::new(
                "lumen_tokens_total",
                "Tokens processed, by capability/model/provider/direction and whether the count was locally estimated (ADR 003).",
            ),
            &token_label_names,
        )?;
        let rerank_search_units_total = IntCounterVec::new(
            Opts::new(
                "lumen_rerank_search_units_total",
                "Rerank search units reported by upstream providers.",
            ),
            &unit_label_names,
        )?;
        let tokens_estimated_total = IntCounter::new(
            "lumen_tokens_estimated_total",
            "Tokens that were locally estimated rather than upstream-reported.",
        )?;
        let usage_log_dropped_total = IntCounter::new(
            "lumen_usage_log_dropped_total",
            "Usage-log entries dropped because the logging channel was full.",
        )?;
        let metadata_rejected_total = IntCounter::new(
            "lumen_metadata_rejected_total",
            "x-lumen-metadata headers dropped as malformed or out of bounds.",
        )?;

        let registry = metrics.registry();
        registry.register(Box::new(tokens_total.clone()))?;
        registry.register(Box::new(rerank_search_units_total.clone()))?;
        registry.register(Box::new(tokens_estimated_total.clone()))?;
        registry.register(Box::new(usage_log_dropped_total.clone()))?;
        registry.register(Box::new(metadata_rejected_total.clone()))?;

        Ok(Self {
            tokens_total,
            rerank_search_units_total,
            tokens_estimated_total,
            usage_log_dropped_total,
            metadata_rejected_total,
            metadata_labels: metadata_labels.to_vec(),
        })
    }

    /// The allowlisted metadata label names, in the order the record methods
    /// expect their values.
    #[must_use]
    pub fn metadata_labels(&self) -> &[String] {
        &self.metadata_labels
    }

    /// Count `count` tokens. `metadata_values` must align with
    /// [`metadata_labels`](Self::metadata_labels) (use `""` for absent keys);
    /// a mismatched length is treated as all-absent rather than panicking.
    pub fn add_tokens(&self, sample: &TokenSample<'_>, metadata_values: &[&str], count: u64) {
        if count == 0 {
            return;
        }
        let mut values: Vec<&str> = vec![
            sample.capability,
            sample.model,
            sample.provider,
            sample.direction.as_str(),
            if sample.estimated { "true" } else { "false" },
        ];
        self.extend_metadata(&mut values, metadata_values);
        self.tokens_total.with_label_values(&values).inc_by(count);
        if sample.estimated {
            self.tokens_estimated_total.inc_by(count);
        }
    }

    /// Count rerank search units.
    pub fn add_search_units(
        &self,
        model: &str,
        provider: &str,
        metadata_values: &[&str],
        count: u64,
    ) {
        if count == 0 {
            return;
        }
        let mut values: Vec<&str> = vec![model, provider];
        self.extend_metadata(&mut values, metadata_values);
        self.rerank_search_units_total
            .with_label_values(&values)
            .inc_by(count);
    }

    /// One usage-log entry was dropped because the channel was full.
    pub fn inc_usage_dropped(&self) {
        self.usage_log_dropped_total.inc();
    }

    /// One metadata header was rejected (malformed / out of bounds).
    pub fn inc_metadata_rejected(&self) {
        self.metadata_rejected_total.inc();
    }

    fn extend_metadata<'a>(&'a self, values: &mut Vec<&'a str>, metadata_values: &[&'a str]) {
        if metadata_values.len() == self.metadata_labels.len() {
            values.extend_from_slice(metadata_values);
        } else {
            // Defensive: a caller bug must corrupt one metric sample at
            // worst, never panic the request path.
            values.extend(std::iter::repeat("").take(self.metadata_labels.len()));
        }
    }
}

/// One token-count observation's label set (everything but the metadata).
#[derive(Debug, Clone, Copy)]
pub struct TokenSample<'a> {
    /// `chat` | `embed` | `rerank`.
    pub capability: &'a str,
    /// Client-facing model id.
    pub model: &'a str,
    /// Provider instance name.
    pub provider: &'a str,
    /// Input or output tokens.
    pub direction: Direction,
    /// Whether the count was locally estimated (ADR 003).
    pub estimated: bool,
}

/// Token direction label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Prompt / input tokens.
    Input,
    /// Completion / output tokens.
    Output,
}

impl Direction {
    /// The Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(labels: &[&str]) -> (Metrics, TokenMetrics) {
        let metrics = Metrics::new();
        let labels: Vec<String> = labels.iter().map(|&s| s.to_owned()).collect();
        let tokens = TokenMetrics::register(&metrics, &labels).expect("register");
        (metrics, tokens)
    }

    fn sample<'a>(
        capability: &'a str,
        model: &'a str,
        provider: &'a str,
        direction: Direction,
        estimated: bool,
    ) -> TokenSample<'a> {
        TokenSample {
            capability,
            model,
            provider,
            direction,
            estimated,
        }
    }

    #[test]
    fn tokens_appear_with_all_base_labels() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_tokens(
            &sample("chat", "gpt-x", "openai", Direction::Input, false),
            &[],
            42,
        );
        let out = metrics.encode_text();
        assert!(out.contains("lumen_tokens_total"));
        assert!(out.contains(r#"capability="chat""#));
        assert!(out.contains(r#"direction="input""#));
        assert!(out.contains(r#"estimated="false""#));
        assert!(out.contains("42"));
    }

    #[test]
    fn estimated_tokens_also_feed_the_estimated_counter() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_tokens(
            &sample("embed", "tei-m", "tei", Direction::Input, true),
            &[],
            7,
        );
        let out = metrics.encode_text();
        assert!(out.contains(r#"estimated="true""#));
        assert!(out.contains("lumen_tokens_estimated_total 7"));
    }

    #[test]
    fn upstream_counts_do_not_touch_the_estimated_counter() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_tokens(&sample("chat", "m", "p", Direction::Output, false), &[], 5);
        let out = metrics.encode_text();
        assert!(out.contains("lumen_tokens_estimated_total 0"));
    }

    #[test]
    fn allowlisted_metadata_becomes_labels() {
        let (metrics, tokens) = setup(&["team", "env"]);
        tokens.add_tokens(
            &sample("chat", "m", "p", Direction::Input, false),
            &["search", "prod"],
            1,
        );
        let out = metrics.encode_text();
        assert!(out.contains(r#"team="search""#));
        assert!(out.contains(r#"env="prod""#));
    }

    #[test]
    fn absent_allowlisted_keys_get_empty_label_values() {
        let (metrics, tokens) = setup(&["team"]);
        tokens.add_tokens(&sample("chat", "m", "p", Direction::Input, false), &[""], 1);
        let out = metrics.encode_text();
        assert!(out.contains(r#"team="""#));
    }

    #[test]
    fn search_units_counter_works() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_search_units("rerank-v3", "cohere", &[], 3);
        let out = metrics.encode_text();
        assert!(out.contains("lumen_rerank_search_units_total"));
        assert!(out.contains(r#"provider="cohere""#));
    }

    #[test]
    fn zero_counts_create_no_series() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_tokens(&sample("chat", "m", "p", Direction::Input, false), &[], 0);
        tokens.add_search_units("m", "p", &[], 0);
        let out = metrics.encode_text();
        assert!(!out.contains(r#"model="m""#));
    }

    #[test]
    fn invalid_metadata_label_name_is_a_registration_error() {
        let metrics = Metrics::new();
        let bad = vec!["not a valid label!".to_owned()];
        assert!(TokenMetrics::register(&metrics, &bad).is_err());
    }

    #[test]
    fn mismatched_metadata_value_count_degrades_to_absent_not_panic() {
        let (metrics, tokens) = setup(&["team"]);
        tokens.add_tokens(&sample("chat", "m", "p", Direction::Input, false), &[], 1);
        let out = metrics.encode_text();
        assert!(out.contains(r#"team="""#));
    }

    #[test]
    fn drop_and_rejection_counters_increment() {
        let (metrics, tokens) = setup(&[]);
        tokens.inc_usage_dropped();
        tokens.inc_metadata_rejected();
        tokens.inc_metadata_rejected();
        let out = metrics.encode_text();
        assert!(out.contains("lumen_usage_log_dropped_total 1"));
        assert!(out.contains("lumen_metadata_rejected_total 2"));
    }
}
