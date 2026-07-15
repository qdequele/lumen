# Usage log & multi-tenant metadata

When `[auth]` is enabled, every request writes a row to the `usage_log`
table: token counts, cost, status, the model that actually served the
request, and metadata - never message content. Prompts and responses are
never logged, by default and in the usage log alike.

## Never on the request path

Usage-log writes never block a request. Each accounted call pushes an entry
onto a bounded async channel (`usage_channel_capacity`); a separate batched
writer task drains it and writes to the database in batches
(`usage_batch_max` entries, or at least every `usage_flush_ms`). If the
channel is ever full, an entry is dropped rather than backpressuring the
request path, and `lumen_usage_log_dropped_total` increments so the drop is
visible on [`/metrics`](metrics.md).

Rows age out on their own: `retention_days` purges `usage_log` rows older
than that many days.

## `x-lumen-metadata`

Clients may attach a per-request metadata header, canonically
`x-lumen-metadata` (alias `cf-aig-metadata`, for drop-in compatibility with
Cloudflare AI Gateway clients). The value is a flat JSON object of
string/number/bool values, bounded so log records and memory stay bounded:
at most 16 keys, each key at most 64 bytes, each value at most 256 bytes,
and the whole header at most 4 KiB.

The full (bounded) object is attached to structured logs and stored in the
`usage_log` `metadata` column for later filtering. It is opaque - LUMEN
never parses it for meaning or PII - and it is logged, so it must never
carry secrets or prompt content.

Missing, malformed, oversized or wrong-typed metadata never fails the
request: it is dropped with a log line and a `lumen_metadata_rejected_total`
increment, and the call proceeds normally. See
[ADR 002](../adr/002-request-metadata-header.md).

## Prometheus label allowlist

Only keys listed in `telemetry.metadata_labels` become Prometheus labels on
the token/media counters; every other key stays logs-only:

```toml
[telemetry]
metadata_labels = []              # e.g. ["team", "env"]
```

The default is empty, which means client-supplied metadata can never mint a
single new Prometheus time series - metric cardinality is a deliberate,
operator-bounded decision, never a client-driven one. An allowlisted key
absent from a given request gets the label value `""`. Keep the value sets
you allowlist bounded: every distinct combination of allowlisted values is
its own time series.

## Multi-tenant recipe

Send org/team/project identifiers in the metadata header, allowlist the
keys you want to slice by, and query per tenant on `/metrics`:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-lumen-metadata: {"org_id":"acme","team_id":"rag","project_id":"docs-chat"}' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
```

```toml
[telemetry]
metadata_labels = ["org_id", "team_id", "project_id"]
```

Tokens per organization over the last 24 hours:

```promql
sum by (org_id) (increase(lumen_tokens_total{org_id!=""}[24h]))
```

The `monitoring/` rig's `traffic.py` script simulates exactly this pattern
across several tenants and its dashboard has a dedicated multi-tenant panel
row - see `monitoring/README.md`.

## Disconnect accounting

A client that disconnects mid-stream is not a gateway failure and must not
be recorded as a fake success. The request's accounting settles at `499`
(`LM-6001`) instead: `usage_log.status` and the
`lumen_request_duration_seconds{status="499"}` sample both reflect the
disconnect, kept out of both the `internal`-error class and the `5xx`
status class so a client hanging up never inflates internal-error alerts.
See [Error codes](../errors.md).
