# ADR 002 — Per-request metadata header for logging & metrics

- Status: accepted (planned for M5)
- Date: 2026-07-12
- Milestone: M5

## Context

Operators running a shared gateway need to attribute and slice observability
data by dimensions the gateway cannot infer: which end-user, team, feature,
environment or experiment a call belongs to. Cloudflare AI Gateway solves this
with a `cf-aig-metadata` request header carrying a small JSON object that is
then attached to each request's log for filtering and search.

LUMEN needs the same capability, feeding both:

- **structured logs** (`tracing`) and the M5 `usage_log` records, and
- **Prometheus** metrics.

The tension is Prometheus **cardinality**. Prometheus label values multiply the
number of time series; putting arbitrary client-supplied metadata (e.g. a
`user_id`) onto metric labels is an unbounded-cardinality footgun that violates
pillar 1 (~15 MB idle, < 1 ms p99) — a few thousand distinct users would blow up
memory and scrape cost. Cloudflare sidesteps this by indexing metadata for
**log search**, not by turning it into metric dimensions.

We also refuse to make an observability header able to fail a real request:
rejecting a chat call because someone sent malformed metadata is user-hostile.

And per the sovereignty pillar, metadata **is** logged, so it must never be
treated as prompt content — it is operator/client labels only.

## Decision

Introduce a per-request metadata header, parsed once at the edge into a small
typed value carried in request extensions.

1. **Header.** Canonical `x-lumen-metadata`; also accept `cf-aig-metadata`
   as an alias so Cloudflare AI Gateway clients work unchanged. Value is a flat
   JSON object of string → (string | number | bool). Nested objects/arrays are
   rejected (dropped, see 4).

2. **Bounds.** At most 16 keys; key ≤ 64 bytes; value ≤ 256 bytes; whole header
   ≤ 4 KiB. These keep log records and memory bounded.

3. **Two sinks, different rules.**
   - **Logs / `usage_log`:** the full (bounded) object is attached to the
     request's structured log fields and stored in a `metadata` column on
     `usage_log` (M5) for later filtering — the Cloudflare-style use case.
   - **Prometheus:** ONLY keys named in a config **allowlist**
     (`telemetry.metadata_labels = ["env", "team"]`, default empty) become
     metric labels; every other key is logs-only. An allowlisted key absent
     from a given request gets the label value `""`. This makes metric
     cardinality a deliberate, operator-bounded decision — never client-driven.

4. **Never fails the request.** Missing, malformed, oversized or wrong-typed
   metadata is dropped with a `debug!`/`warn!` and a
   `metadata_rejected_total` counter increment; the call proceeds normally.

5. **Opaque, never inspected.** LUMEN does not parse metadata for meaning or
   PII. Documentation states plainly that metadata is logged and must not carry
   secrets or prompt content.

## Consequences

- Metric cardinality is capped by config, not by traffic — safe by default
  (empty allowlist = zero new label dimensions).
- Full metadata is still available for rich filtering via `usage_log`, matching
  Cloudflare's log-search model.
- The request path gains only a bounded header parse (no allocation when the
  header is absent), preserving the latency pillar.
- Implementation lands in M5 alongside usage logging (`usage_log.metadata`
  column, the batched writer, and the Prometheus label wiring in `telemetry`).
  A thin extractor in `server` reads the header into request extensions so chat,
  embeddings and rerank handlers all share it.
