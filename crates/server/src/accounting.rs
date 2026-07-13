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
use ferrogate_auth::state::{usd_to_micro, Reservation};
use ferrogate_auth::store::UsageRecord;
use ferrogate_auth::usage::UsageLogger;
use ferrogate_core::GatewayError;
use ferrogate_telemetry::tokens::{Direction, TokenMetrics, TokenSample};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

/// The measured result of one finished request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Outcome {
    /// Input/prompt tokens.
    pub tokens_in: u64,
    /// Output/completion tokens.
    pub tokens_out: u64,
    /// Whether the counts were locally estimated (ADR 003).
    pub estimated: bool,
    /// Rerank search units, when applicable.
    pub search_units: Option<u64>,
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
    usage: Option<UsageLogger>,
    pricing: Arc<CostTable>,
    started: Instant,
}

impl Accounting {
    /// Parse the request metadata, admit the request against the key's
    /// budget/quotas (when auth is on), and open the accounting record.
    ///
    /// # Errors
    ///
    /// Budget/quota rejections from admission — decided in memory, BEFORE
    /// any upstream call. Metadata problems never error (ADR 002): they are
    /// dropped with a warn + counter.
    pub fn begin(
        state: &AppState,
        headers: &HeaderMap,
        key: Option<&AuthedKey>,
        target: Target<'_>,
        estimated_tokens: u64,
        estimated_cost: f64,
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
            Some(AuthedKey(entry)) => {
                let reservation = entry.admit(
                    now_unix(),
                    i64::try_from(estimated_tokens).unwrap_or(i64::MAX),
                    usd_to_micro(estimated_cost),
                )?;
                (Some(entry.id().to_owned()), Some(reservation))
            }
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
            usage: state.usage.clone(),
            pricing: Arc::clone(&state.pricing),
            started: Instant::now(),
        })
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
            reservation.settle(usd_to_micro(outcome.cost));
        }

        let metadata_json = self.metadata.as_ref().map(RequestMetadata::to_json);

        // ADR 002 sink 1, log half: the full metadata rides the structured
        // usage event (labels only — never prompt or response content).
        tracing::debug!(
            target: "ferrogate::usage",
            capability = self.capability,
            model = %self.model,
            model_used = %self.model_used,
            provider = %self.provider,
            key_id = self.key_id.as_deref().unwrap_or("-"),
            tokens_in = outcome.tokens_in,
            tokens_out = outcome.tokens_out,
            estimated = outcome.estimated,
            cost = outcome.cost,
            status = outcome.status,
            metadata = metadata_json.as_deref().unwrap_or("{}"),
            "request usage"
        );

        let allowlist = self.tokens.metadata_labels();
        let values: Vec<&str> = match &self.metadata {
            Some(meta) => meta.label_values(allowlist),
            None => crate::metadata::empty_label_values(allowlist),
        };

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

        if let Some(logger) = &self.usage {
            let record = UsageRecord {
                key_id: self.key_id.clone(),
                model: self.model.clone(),
                model_used: self.model_used.clone(),
                capability: self.capability.to_owned(),
                tokens_in: i64::try_from(outcome.tokens_in).unwrap_or(i64::MAX),
                tokens_out: i64::try_from(outcome.tokens_out).unwrap_or(i64::MAX),
                search_units: outcome
                    .search_units
                    .map(|u| i64::try_from(u).unwrap_or(i64::MAX)),
                estimated: outcome.estimated,
                cost: outcome.cost,
                latency_ms: i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX),
                status: outcome.status,
                metadata: metadata_json,
                ts: now_unix(),
            };
            if !logger.log(record) {
                // Channel full: drop the entry, count it, move on (§5.3).
                self.tokens.inc_usage_dropped();
            }
        }
    }
}

/// Streaming accounting: sniffs the forwarded SSE bytes for the final usage
/// chunk and closes the [`Accounting`] when the stream ends — including on
/// client disconnect (via `Drop`), where whatever was observed so far is
/// settled instead of refunding tokens the upstream already produced.
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

    /// Close the record with a 200 outcome (clean end or client disconnect —
    /// the HTTP status the client saw). Idempotent; also runs from `Drop`.
    pub fn finalize(&mut self) {
        self.finalize_with_status(200);
    }

    /// Close the record with the terminal outcome of the stream: 200 for a
    /// clean end, the gateway error's status (502/504/…) when the stream was
    /// cut short by an in-band error frame — so `usage_log.status` reflects
    /// what actually happened, not the initial response headers.
    pub fn finalize_with_status(&mut self, status: u16) {
        let Some(accounting) = self.accounting.take() else {
            return;
        };
        let (tokens_in, tokens_out, estimated) = self.sniffer.result(self.estimated_input);
        let cost = accounting
            .pricing()
            .token_cost(&accounting.model, tokens_in, tokens_out);
        accounting.finish(&Outcome {
            tokens_in,
            tokens_out,
            estimated,
            search_units: None,
            cost,
            status,
        });
    }
}

impl Drop for StreamAccounting {
    fn drop(&mut self) {
        self.finalize();
    }
}

/// Maximum retained line length; SSE data lines beyond this are skipped (the
/// stream itself is untouched — we only lose the ability to read its usage).
const MAX_SNIFFED_LINE: usize = 256 * 1024;

/// Scans forwarded SSE bytes, line by line, retaining ONLY the latest valid
/// top-level usage object plus a frame count — bounded state, no response
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
    /// and the data-frame count (streaming deltas are roughly one token each)
    /// — honestly flagged estimated, never a silent zero.
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
        // Model content mentions "usage" — retained, but the real final
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
