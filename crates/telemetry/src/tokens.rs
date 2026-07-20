//! Token-accounting Prometheus counters (ADR 003) and the ADR 002
//! metadata-label plumbing.
//!
//! Cardinality is fixed by construction: the base labels are enums or
//! operator-configured model/provider names, and metadata labels exist only
//! for keys the operator explicitly allowlisted (`telemetry.metadata_labels`,
//! default empty) - never for arbitrary client-supplied keys.

use crate::metrics::Metrics;
use prometheus::{IntCounter, IntCounterVec, Opts};

/// Base label names of `lumen_tokens_total`.
const TOKEN_LABELS: [&str; 5] = ["capability", "model", "provider", "direction", "estimated"];
/// Base label names of `lumen_rerank_search_units_total`.
const SEARCH_UNIT_LABELS: [&str; 2] = ["model", "provider"];
/// Base label names of the media counters (M9).
const MEDIA_LABELS: [&str; 4] = ["capability", "model", "provider", "media_type"];
/// Base label names of `lumen_token_breakdown_total` (issue #99).
const BREAKDOWN_LABELS: [&str; 4] = ["capability", "model", "provider", "kind"];

/// All M5 token/usage counters, registered against one [`Metrics`] registry.
#[derive(Debug, Clone)]
pub struct TokenMetrics {
    tokens_total: IntCounterVec,
    rerank_search_units_total: IntCounterVec,
    media_total: IntCounterVec,
    media_bytes_total: IntCounterVec,
    /// Cached / reasoning / cache-write token breakdown (issue #99). A subset
    /// of `tokens_total`, split out by `kind` so it can be summed independently
    /// without inflating the primary token counter.
    token_breakdown_total: IntCounterVec,
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
        let media_label_names: Vec<&str> = MEDIA_LABELS
            .iter()
            .copied()
            .chain(metadata_labels.iter().map(String::as_str))
            .collect();
        let breakdown_label_names: Vec<&str> = BREAKDOWN_LABELS
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
        let media_total = IntCounterVec::new(
            Opts::new(
                "lumen_media_total",
                "Media items (images, …) processed, by capability/model/provider/media_type (M9). A billing dimension alongside tokens.",
            ),
            &media_label_names,
        )?;
        let media_bytes_total = IntCounterVec::new(
            Opts::new(
                "lumen_media_bytes_total",
                "Decoded media bytes processed, by capability/model/provider/media_type (M9).",
            ),
            &media_label_names,
        )?;
        let token_breakdown_total = IntCounterVec::new(
            Opts::new(
                "lumen_token_breakdown_total",
                "Upstream-reported token breakdown by kind (cached / reasoning / cache_write), a subset of lumen_tokens_total (issue #99).",
            ),
            &breakdown_label_names,
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
        registry.register(Box::new(media_total.clone()))?;
        registry.register(Box::new(media_bytes_total.clone()))?;
        registry.register(Box::new(token_breakdown_total.clone()))?;
        registry.register(Box::new(tokens_estimated_total.clone()))?;
        registry.register(Box::new(usage_log_dropped_total.clone()))?;
        registry.register(Box::new(metadata_rejected_total.clone()))?;

        Ok(Self {
            tokens_total,
            rerank_search_units_total,
            media_total,
            media_bytes_total,
            token_breakdown_total,
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

    /// Count `count` media items totalling `bytes` decoded bytes, for one
    /// top-level `media_type` (M9). `metadata_values` aligns with
    /// [`metadata_labels`](Self::metadata_labels). A zero count is a no-op (no
    /// series created), mirroring the token counters.
    pub fn add_media(
        &self,
        sample: &MediaSample<'_>,
        metadata_values: &[&str],
        count: u64,
        bytes: u64,
    ) {
        if count == 0 {
            return;
        }
        let mut values: Vec<&str> = vec![
            sample.capability,
            sample.model,
            sample.provider,
            sample.media_type,
        ];
        self.extend_metadata(&mut values, metadata_values);
        self.media_total.with_label_values(&values).inc_by(count);
        self.media_bytes_total
            .with_label_values(&values)
            .inc_by(bytes);
    }

    /// Count `count` tokens of one breakdown `kind` (issue #99). A zero count
    /// is a no-op (no series created), mirroring the other token counters, so
    /// an absent upstream breakdown never fabricates a `0` series (ADR 003).
    pub fn add_token_breakdown(
        &self,
        sample: &BreakdownSample<'_>,
        metadata_values: &[&str],
        count: u64,
    ) {
        if count == 0 {
            return;
        }
        let mut values: Vec<&str> = vec![
            sample.capability,
            sample.model,
            sample.provider,
            sample.kind.as_str(),
        ];
        self.extend_metadata(&mut values, metadata_values);
        self.token_breakdown_total
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

/// One media observation's label set (everything but the metadata).
#[derive(Debug, Clone, Copy)]
pub struct MediaSample<'a> {
    /// `chat` | `embed` | `rerank`.
    pub capability: &'a str,
    /// Client-facing model id.
    pub model: &'a str,
    /// Provider instance name.
    pub provider: &'a str,
    /// Top-level media type (`"image"`, `"audio"`, …).
    pub media_type: &'a str,
}

/// One token-breakdown observation's label set (issue #99).
#[derive(Debug, Clone, Copy)]
pub struct BreakdownSample<'a> {
    /// `chat` | `embed` | `rerank`.
    pub capability: &'a str,
    /// Client-facing model id.
    pub model: &'a str,
    /// Provider instance name.
    pub provider: &'a str,
    /// Which breakdown this count belongs to.
    pub kind: BreakdownKind,
}

/// The `kind` label of `lumen_token_breakdown_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakdownKind {
    /// Prompt tokens served from cache (cache read / hit).
    Cached,
    /// Reasoning tokens billed within the completion.
    Reasoning,
    /// Prompt tokens written to cache (cache write, Anthropic cache-creation).
    CacheWrite,
}

impl BreakdownKind {
    /// The Prometheus label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BreakdownKind::Cached => "cached",
            BreakdownKind::Reasoning => "reasoning",
            BreakdownKind::CacheWrite => "cache_write",
        }
    }
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
    fn media_counters_record_count_and_bytes() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_media(
            &MediaSample {
                capability: "embed",
                model: "voyage-mm",
                provider: "voyage",
                media_type: "image",
            },
            &[],
            2,
            2048,
        );
        let out = metrics.encode_text();
        assert!(out.contains("lumen_media_total"));
        assert!(out.contains("lumen_media_bytes_total"));
        assert!(out.contains(r#"media_type="image""#));
        assert!(out.contains(r#"capability="embed""#));
    }

    #[test]
    fn zero_media_count_creates_no_series() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_media(
            &MediaSample {
                capability: "embed",
                model: "m",
                provider: "p",
                media_type: "image",
            },
            &[],
            0,
            0,
        );
        assert!(!metrics.encode_text().contains("lumen_media_total{"));
    }

    #[test]
    fn breakdown_counter_records_kind_and_base_labels() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_token_breakdown(
            &BreakdownSample {
                capability: "chat",
                model: "gpt-5",
                provider: "openai",
                kind: BreakdownKind::Cached,
            },
            &[],
            64,
        );
        let out = metrics.encode_text();
        assert!(out.contains("lumen_token_breakdown_total"));
        assert!(out.contains(r#"kind="cached""#));
        assert!(out.contains(r#"model="gpt-5""#));
        assert!(out.contains("64"));
    }

    #[test]
    fn zero_breakdown_count_creates_no_series() {
        let (metrics, tokens) = setup(&[]);
        tokens.add_token_breakdown(
            &BreakdownSample {
                capability: "chat",
                model: "m",
                provider: "p",
                kind: BreakdownKind::Reasoning,
            },
            &[],
            0,
        );
        assert!(!metrics
            .encode_text()
            .contains("lumen_token_breakdown_total{"));
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
