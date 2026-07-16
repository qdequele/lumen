//! Per-request accounting: budget admission, token counting (ADR 003), cost
//! derivation (M5 §5.4b), metadata sinks (ADR 002) and the usage log.
//!
//! One [`Accounting`] is opened per request after routing and closed exactly
//! once with the observed outcome. Everything it does is hot-path-safe:
//! admission is an in-memory CAS, counting is arithmetic, metadata was
//! bounded at parse time, and the usage log is a non-blocking `try_send`.
//! Dropping it without finishing (upstream failure, client disconnect before
//! completion) refunds the budget reservation via [`Reservation`]'s drop.

use crate::auth::{now_unix, AuthedKey};
use crate::metadata::{MetadataOutcome, RequestMetadata};
use crate::pricing::CostTable;
use crate::state::AppState;
use axum::http::HeaderMap;
use lumen_auth::state::{usd_to_micro, Reservation};
use lumen_auth::store::UsageRecord;
use lumen_auth::usage::UsageLogger;
use lumen_core::GatewayError;
use lumen_telemetry::tokens::{Direction, TokenMetrics, TokenSample};
use lumen_telemetry::LatencyMetrics;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The measured result of one finished request.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Input/prompt tokens.
    pub tokens_in: u64,
    /// Output/completion tokens.
    pub tokens_out: u64,
    /// Whether the counts were locally estimated (ADR 003).
    pub estimated: bool,
    /// Rerank search units, when applicable.
    pub search_units: Option<u64>,
    /// Media accounting (count + decoded bytes, by type) - M9. Empty for
    /// text-only requests.
    pub media: lumen_core::MediaUsage,
    /// Cost in USD (from the config price table).
    pub cost: f64,
    /// HTTP status returned to the client.
    pub status: u16,
}

/// What is being called: capability, client-facing model id, provider name.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// `chat` | `embed` | `rerank`.
    pub capability: &'static str,
    /// Client-facing model id (the aliased id, not the upstream one).
    pub model: &'a str,
    /// Provider instance name.
    pub provider: &'a str,
}

/// Open accounting for one admitted request.
pub struct Accounting {
    capability: &'static str,
    /// Client-facing model id the client requested (the aliased id).
    model: String,
    /// Model that actually served the request (a fallback may differ). Set via
    /// [`served_by`](Accounting::served_by) after the executor resolves it;
    /// defaults to the requested model.
    model_used: String,
    provider: String,
    key_id: Option<String>,
    reservation: Option<Reservation>,
    metadata: Option<RequestMetadata>,
    tokens: TokenMetrics,
    latency: LatencyMetrics,
    usage: Option<UsageLogger>,
    pricing: Arc<CostTable>,
    started: Instant,
    /// End-to-end latency frozen at response time (see
    /// [`mark_completed`](Accounting::mark_completed)); `None` = measure at
    /// [`finish`](Accounting::finish).
    completed: Option<Duration>,
}

impl Accounting {
    /// Parse the request metadata, admit the request against the key's
    /// budget/quotas (when auth is on), and open the accounting record.
    ///
    /// # Errors
    ///
    /// Budget/quota rejections from admission - decided in memory, BEFORE
    /// any upstream call. Metadata problems never error (ADR 002): they are
    /// dropped with a warn + counter.
    pub fn begin(
        state: &AppState,
        headers: &HeaderMap,
        key: Option<&AuthedKey>,
        target: Target<'_>,
        estimated_tokens: u64,
        estimated_cost: f64,
        pricing: Arc<CostTable>,
    ) -> Result<Self, GatewayError> {
        let metadata = match RequestMetadata::extract(headers) {
            MetadataOutcome::Absent => None,
            MetadataOutcome::Valid(meta) => Some(meta),
            MetadataOutcome::Rejected(reason) => {
                // The request itself proceeds normally (ADR 002 rule 4).
                tracing::warn!(reason, "request metadata dropped");
                state.tokens.inc_metadata_rejected();
                None
            }
        };

        let (key_id, reservation) = match key {
            Some(AuthedKey(entry)) => match entry.admit(
                now_unix(),
                i64::try_from(estimated_tokens).unwrap_or(i64::MAX),
                usd_to_micro(estimated_cost),
            ) {
                Ok(reservation) => (Some(entry.id().to_owned()), Some(reservation)),
                Err(error) => {
                    // The request is refused (402/429) before any upstream
                    // call. Still record a status-only usage-log row so
                    // per-key rejection analytics work, via the same
                    // non-blocking channel as successful requests (never a
                    // synchronous DB write on the request path).
                    Self::log_rejection(
                        state,
                        target,
                        entry.id(),
                        metadata.as_ref(),
                        error.http_status(),
                    );
                    return Err(error);
                }
            },
            None => (None, None),
        };

        Ok(Self {
            capability: target.capability,
            model: target.model.to_owned(),
            model_used: target.model.to_owned(),
            provider: target.provider.to_owned(),
            key_id,
            reservation,
            metadata,
            tokens: state.tokens.clone(),
            latency: state.latency.clone(),
            usage: state.usage.clone(),
            pricing,
            started: Instant::now(),
            completed: None,
        })
    }

    /// Enqueue a status-only usage-log row for a request refused at admission
    /// (budget/quota). Zero tokens and zero cost - nothing was consumed - the
    /// `status` column (402/429) carries the rejection. Non-blocking: a full
    /// channel drops the row and counts it, exactly like the success path.
    fn log_rejection(
        state: &AppState,
        target: Target<'_>,
        key_id: &str,
        metadata: Option<&RequestMetadata>,
        status: u16,
    ) {
        let Some(logger) = &state.usage else {
            return;
        };
        let record = UsageRecord {
            key_id: Some(key_id.to_owned()),
            model: target.model.to_owned(),
            model_used: target.model.to_owned(),
            provider: target.provider.to_owned(),
            capability: target.capability.to_owned(),
            tokens_in: 0,
            tokens_out: 0,
            search_units: None,
            media_count: 0,
            media_bytes: 0,
            estimated: false,
            cost: 0.0,
            latency_ms: 0,
            status,
            metadata: metadata.map(RequestMetadata::to_json),
            ts: now_unix(),
        };
        if !logger.log(record) {
            state.tokens.inc_usage_dropped();
        }
    }

    /// Freeze the request's end-to-end latency NOW. Called by a handler that
    /// defers [`finish`](Accounting::finish) to a background task (opt-in
    /// accurate token refinement, ADR 003), so the latency histogram and
    /// `usage_log.latency_ms` measure the request the client saw, not the
    /// deferred refinement.
    pub fn mark_completed(&mut self) {
        self.completed = Some(self.started.elapsed());
    }

    /// The shared price table (for computing the outcome's cost).
    #[must_use]
    pub fn pricing(&self) -> &CostTable {
        &self.pricing
    }

    /// Record which provider/model actually served the request (M6): a fallback
    /// may differ from the primary. Drives the token-metric `model`/`provider`
    /// labels, the cost model and `usage_log.model_used`.
    pub fn served_by(&mut self, model_used: &str, provider: &str) {
        model_used.clone_into(&mut self.model_used);
        provider.clone_into(&mut self.provider);
    }

    /// The model that served the request (for the caller's cost calculation).
    #[must_use]
    pub fn model_used(&self) -> &str {
        &self.model_used
    }

    /// Close the record: settle the budget reservation at the real cost,
    /// bump the Prometheus counters and enqueue the usage-log entry. Never
    /// fails and never blocks (ADR 003 hot-path rule).
    pub fn finish(mut self, outcome: &Outcome) {
        if let Some(reservation) = self.reservation.take() {
            // Settle both dimensions to the real usage: the budget to the real
            // cost and the TPM window to the real token count (in - out).
            let actual_tokens = i64::try_from(outcome.tokens_in.saturating_add(outcome.tokens_out))
                .unwrap_or(i64::MAX);
            reservation.settle(usd_to_micro(outcome.cost), actual_tokens);
        }

        // One clock read closes the record: the log event, the histogram and
        // `usage_log.latency_ms` all report the same measurement. Streaming
        // finishes when the stream ends, so this covers the full stream. A
        // handler that deferred this close to a background refinement task
        // froze the latency at response time via `mark_completed`.
        let elapsed = self.completed.unwrap_or_else(|| self.started.elapsed());
        let metadata_json = self.metadata.as_ref().map(RequestMetadata::to_json);

        // ADR 002 sink 1, log half: the full metadata rides the structured
        // usage event (labels only - never prompt or response content).
        tracing::debug!(
            target: "lumen::usage",
            capability = self.capability,
            model = %self.model,
            model_used = %self.model_used,
            provider = %self.provider,
            key_id = self.key_id.as_deref().unwrap_or("-"),
            tokens_in = outcome.tokens_in,
            tokens_out = outcome.tokens_out,
            media_count = outcome.media.count,
            media_bytes = outcome.media.bytes,
            estimated = outcome.estimated,
            cost = outcome.cost,
            latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            status = outcome.status,
            metadata = metadata_json.as_deref().unwrap_or("{}"),
            "request usage"
        );

        // End-to-end latency attributed to the model/provider that actually
        // served the request - the analytics counterpart of the HTTP-level
        // histogram, which for streams only sees time-to-headers.
        self.latency.observe_request(
            self.capability,
            &self.model_used,
            &self.provider,
            outcome.status,
            elapsed.as_secs_f64(),
        );

        let allowlist = self.tokens.metadata_labels();
        let owned_values = match &self.metadata {
            Some(meta) => meta.label_values(allowlist),
            None => crate::metadata::empty_label_values(allowlist),
        };
        let values: Vec<&str> = owned_values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect();

        // Metrics attribute tokens to the model/provider that actually served
        // the request (== requested unless a fallback fired).
        self.tokens.add_tokens(
            &TokenSample {
                capability: self.capability,
                model: &self.model_used,
                provider: &self.provider,
                direction: Direction::Input,
                estimated: outcome.estimated,
            },
            &values,
            outcome.tokens_in,
        );
        self.tokens.add_tokens(
            &TokenSample {
                capability: self.capability,
                model: &self.model_used,
                provider: &self.provider,
                direction: Direction::Output,
                estimated: outcome.estimated,
            },
            &values,
            outcome.tokens_out,
        );
        if let Some(units) = outcome.search_units {
            self.tokens
                .add_search_units(&self.model_used, &self.provider, &values, units);
        }
        // M9: media count + decoded bytes, one Prometheus sample per top-level
        // media type, attributed to the served model/provider.
        for ty in &outcome.media.by_type {
            self.tokens.add_media(
                &lumen_telemetry::tokens::MediaSample {
                    capability: self.capability,
                    model: &self.model_used,
                    provider: &self.provider,
                    media_type: &ty.media_type,
                },
                &values,
                ty.count,
                ty.bytes,
            );
        }

        self.enqueue_usage(outcome, elapsed, metadata_json);
    }

    /// Build and enqueue the usage-log row for a finished request. Non-blocking
    /// `try_send`; a full channel drops the row and counts it (§5.3).
    fn enqueue_usage(
        &self,
        outcome: &Outcome,
        elapsed: std::time::Duration,
        metadata_json: Option<String>,
    ) {
        let Some(logger) = &self.usage else {
            return;
        };
        let record = UsageRecord {
            key_id: self.key_id.clone(),
            model: self.model.clone(),
            model_used: self.model_used.clone(),
            provider: self.provider.clone(),
            capability: self.capability.to_owned(),
            tokens_in: i64::try_from(outcome.tokens_in).unwrap_or(i64::MAX),
            tokens_out: i64::try_from(outcome.tokens_out).unwrap_or(i64::MAX),
            search_units: outcome
                .search_units
                .map(|u| i64::try_from(u).unwrap_or(i64::MAX)),
            media_count: i64::try_from(outcome.media.count).unwrap_or(i64::MAX),
            media_bytes: i64::try_from(outcome.media.bytes).unwrap_or(i64::MAX),
            estimated: outcome.estimated,
            cost: outcome.cost,
            latency_ms: i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
            status: outcome.status,
            metadata: metadata_json,
            ts: now_unix(),
        };
        if !logger.log(record) {
            self.tokens.inc_usage_dropped();
        }
    }
}

/// Streaming accounting: sniffs the forwarded SSE bytes for the final usage
/// chunk and closes the [`Accounting`] when the stream ends - including on
/// client disconnect (via `Drop`, settled as 499 / client-cancelled), where
/// whatever was observed so far is settled instead of refunding tokens the
/// upstream already produced.
pub struct StreamAccounting {
    accounting: Option<Accounting>,
    sniffer: UsageSniffer,
    estimated_input: u64,
}

impl StreamAccounting {
    /// Wrap an open accounting record around a streaming response.
    #[must_use]
    pub fn new(accounting: Accounting, estimated_input: u64) -> Self {
        Self {
            accounting: Some(accounting),
            sniffer: UsageSniffer::default(),
            estimated_input,
        }
    }

    /// Feed one forwarded frame.
    pub fn scan(&mut self, frame: &[u8]) {
        self.sniffer.scan(frame);
    }

    /// Close the record with the terminal outcome of the stream: 200 for a
    /// clean end, the gateway error's status (502/504/…) when the stream was
    /// cut short by an in-band error frame, 499 when the client disconnected
    /// mid-stream (via `Drop`) - so `usage_log.status` reflects what actually
    /// happened, not the initial response headers. Idempotent.
    pub fn finalize_with_status(&mut self, status: u16) {
        let Some(accounting) = self.accounting.take() else {
            return;
        };
        let (tokens_in, tokens_out, estimated) = self.sniffer.result(self.estimated_input);
        // Bill at the model that actually served the stream (a fallback may
        // differ from the requested model) - consistent with the non-streaming,
        // embed and rerank paths.
        let cost = accounting
            .pricing()
            .token_cost(accounting.model_used(), tokens_in, tokens_out);
        accounting.finish(&Outcome {
            tokens_in,
            tokens_out,
            estimated,
            search_units: None,
            media: lumen_core::MediaUsage::default(),
            cost,
            status,
        });
    }
}

impl Drop for StreamAccounting {
    /// The safety net for streams dropped before their terminal event. Every
    /// clean end and in-band error settles explicitly first (idempotent), so
    /// reaching here with an open record means the body was dropped
    /// mid-stream - a client disconnect (or, rarely, server shutdown).
    /// Settled as 499 / client-cancelled (issue #11): never a fake 200
    /// success, never an internal 500.
    fn drop(&mut self) {
        self.finalize_with_status(GatewayError::ClientCancelled.http_status());
    }
}

/// Maximum retained line length; SSE data lines beyond this are skipped (the
/// stream itself is untouched - we only lose the ability to read its usage).
const MAX_SNIFFED_LINE: usize = 256 * 1024;

/// Scans forwarded SSE bytes, line by line, retaining ONLY the latest valid
/// top-level usage object plus a frame count - bounded state, no response
/// accumulation (ADR 004), and serde only on the rare candidate lines.
#[derive(Debug, Default)]
struct UsageSniffer {
    partial: Vec<u8>,
    discarding: bool,
    /// `(prompt_tokens, completion_tokens)` from the last frame carrying a
    /// genuine top-level `usage` object.
    upstream_usage: Option<(u64, u64)>,
    data_frames: u64,
}

impl UsageSniffer {
    fn scan(&mut self, mut chunk: &[u8]) {
        while let Some(newline) = chunk.iter().position(|&b| b == b'\n') {
            if self.discarding {
                // An oversized line just ended: resume normal scanning.
                self.discarding = false;
                self.partial.clear();
            } else {
                self.push_bytes(&chunk[..newline]);
                self.finish_line();
            }
            chunk = &chunk[newline + 1..];
        }
        if !self.discarding {
            self.push_bytes(chunk);
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if self.partial.len().saturating_add(bytes.len()) > MAX_SNIFFED_LINE {
            self.partial.clear();
            self.discarding = true;
        } else {
            self.partial.extend_from_slice(bytes);
        }
    }

    fn finish_line(&mut self) {
        let line: &[u8] = if self.partial.last() == Some(&b'\r') {
            &self.partial[..self.partial.len() - 1]
        } else {
            &self.partial
        };
        if let Some(payload) = line.strip_prefix(b"data:") {
            let payload = if payload.first() == Some(&b' ') {
                &payload[1..]
            } else {
                payload
            };
            if payload != b"[DONE]" && !payload.is_empty() {
                self.data_frames += 1;
                // serde runs only on candidate lines (usually exactly one
                // per stream); a content frame that merely CONTAINS the text
                // "usage" fails the parse and cannot shadow the real chunk.
                if contains(payload, b"\"usage\"") {
                    if let Some(parsed) = parse_usage(payload) {
                        self.upstream_usage = Some(parsed);
                    }
                }
            }
        }
        self.partial.clear();
    }

    /// `(tokens_in, tokens_out, estimated)`. Upstream usage from the last
    /// usage-bearing chunk wins; otherwise the pre-computed input estimate
    /// and the data-frame count (streaming deltas are roughly one token
    /// each) - honestly flagged estimated, never a silent zero.
    fn result(&self, estimated_input: u64) -> (u64, u64, bool) {
        match self.upstream_usage {
            Some((prompt, completion)) => (prompt, completion, false),
            None => (estimated_input, self.data_frames, true),
        }
    }
}

/// Extract `(prompt_tokens, completion_tokens)` from one SSE payload iff it
/// carries a genuine top-level `usage` object with at least one count.
fn parse_usage(payload: &[u8]) -> Option<(u64, u64)> {
    let value = serde_json::from_slice::<Value>(payload).ok()?;
    let usage = value.get("usage").filter(|u| u.is_object())?;
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64);
    let completion = usage.get("completion_tokens").and_then(Value::as_u64);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    Some((prompt.unwrap_or(0), completion.unwrap_or(0)))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_telemetry::Metrics;

    /// A minimal open accounting record wired to a fresh registry, so tests
    /// can observe exactly what closing it records.
    fn open_accounting(metrics: &Metrics) -> Accounting {
        let tokens = TokenMetrics::register(metrics, &[]).expect("register token metrics");
        let latency = LatencyMetrics::register(metrics).expect("register latency metrics");
        Accounting {
            capability: "chat",
            model: "gpt".to_owned(),
            model_used: "gpt".to_owned(),
            provider: "openai".to_owned(),
            key_id: None,
            reservation: None,
            metadata: None,
            tokens,
            latency,
            usage: None,
            pricing: Arc::new(CostTable::default()),
            started: Instant::now(),
            completed: None,
        }
    }

    // Issue #11: the Drop safety net fires exactly when the body stream was
    // dropped BEFORE its terminal event (clean ends and in-band errors all
    // settle explicitly first) - i.e. a mid-stream client disconnect. That
    // must be recorded as 499 / client-cancelled, never as a fake 200
    // success and never as an internal 500.
    #[test]
    fn dropping_an_unfinished_stream_settles_as_a_499_client_cancel() {
        let metrics = Metrics::new();
        let mut stream = StreamAccounting::new(open_accounting(&metrics), 7);
        stream.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        drop(stream);

        let out = metrics.encode_text();
        let line = out
            .lines()
            .find(|l| l.starts_with("lumen_request_duration_seconds_count"))
            .unwrap_or_else(|| panic!("no request duration sample:\n{out}"));
        assert!(line.contains(r#"status="499""#), "{line}");
        assert!(!out.contains(r#"status="200""#), "{out}");
        assert!(!out.contains(r#"status="500""#), "{out}");
    }

    // An explicitly settled stream (clean end or in-band error) must be
    // untouched by the Drop safety net: finalize_with_status is idempotent.
    #[test]
    fn explicit_settlement_wins_over_the_drop_safety_net() {
        let metrics = Metrics::new();
        let mut stream = StreamAccounting::new(open_accounting(&metrics), 7);
        stream.finalize_with_status(200);
        drop(stream);

        let out = metrics.encode_text();
        assert!(out.contains(r#"status="200""#), "{out}");
        assert!(!out.contains(r#"status="499""#), "{out}");
    }

    #[test]
    fn sniffer_reads_usage_from_the_final_chunk() {
        let mut s = UsageSniffer::default();
        s.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        s.scan(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":34}}\n\n",
        );
        s.scan(b"data: [DONE]\n\n");
        assert_eq!(s.result(99), (12, 34, false));
    }

    #[test]
    fn sniffer_survives_frames_split_mid_line() {
        let mut s = UsageSniffer::default();
        s.scan(b"data: {\"usage\":{\"prompt_tok");
        s.scan(b"ens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n");
        assert_eq!(s.result(99), (7, 3, false));
    }

    #[test]
    fn missing_usage_falls_back_to_estimates() {
        let mut s = UsageSniffer::default();
        s.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n");
        s.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n");
        s.scan(b"data: [DONE]\n\n");
        // 2 data frames ≈ 2 output tokens; input from the request estimate.
        assert_eq!(s.result(42), (42, 2, true));
    }

    #[test]
    fn spoofed_usage_in_content_is_overridden_by_the_real_final_chunk() {
        let mut s = UsageSniffer::default();
        // Model content mentions "usage" - retained, but the real final
        // usage chunk arrives later and wins (last occurrence).
        s.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"\\\"usage\\\" is fun\"}}]}\n\n");
        s.scan(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n");
        assert_eq!(s.result(99), (5, 6, false));
    }

    #[test]
    fn content_frame_mentioning_usage_after_the_real_chunk_does_not_degrade_it() {
        // Translated providers may emit the usage chunk before message_stop;
        // a later content-ish frame containing the text "usage" must not
        // shadow the real counts back into an estimate.
        let mut s = UsageSniffer::default();
        s.scan(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n");
        s.scan(b"data: {\"choices\":[{\"delta\":{\"content\":\"my \\\"usage\\\" story\"}}]}\n\n");
        s.scan(b"data: [DONE]\n\n");
        assert_eq!(s.result(99), (5, 6, false));
    }

    #[test]
    fn usage_null_is_not_upstream_usage() {
        let mut s = UsageSniffer::default();
        s.scan(b"data: {\"choices\":[],\"usage\":null}\n\n");
        assert_eq!(s.result(10), (10, 1, true));
    }

    #[test]
    fn oversized_lines_are_skipped_without_breaking_later_lines() {
        let mut s = UsageSniffer::default();
        let huge = vec![b'x'; MAX_SNIFFED_LINE + 10];
        s.scan(b"data: ");
        s.scan(&huge);
        s.scan(b"\n");
        s.scan(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n");
        assert_eq!(s.result(9), (1, 2, false));
    }

    #[test]
    fn done_and_comment_lines_do_not_count_as_frames() {
        let mut s = UsageSniffer::default();
        s.scan(b": ping\n\ndata: [DONE]\n\n");
        assert_eq!(s.result(4), (4, 0, true));
    }
}
