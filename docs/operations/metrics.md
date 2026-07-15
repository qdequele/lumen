# Metrics & dashboards

`GET /metrics` exposes every gateway metric in Prometheus text exposition
format. It is unauthenticated by design - restrict it at the network layer
(firewall, reverse proxy, service mesh) rather than expecting the gateway
to gate it. See [`SECURITY.md`](https://github.com/qdequele/lumen/blob/main/SECURITY.md).

## Metrics reference

| Metric | Labels | Meaning |
|---|---|---|
| `lumen_tokens_total` | `capability, model, provider, direction, estimated` | Tokens processed, cumulative. `direction` is `input`/`output`; `estimated` is `true`/`false` (ADR 003). |
| `lumen_tokens_estimated_total` | none | Subset of the above that was locally estimated rather than upstream-reported. |
| `lumen_rerank_search_units_total` | `model, provider` | Rerank search units, upstream-reported when available. |
| `lumen_media_total` | `capability, model, provider, media_type` | Media items (images, ...) processed - a billing dimension alongside tokens (M9). |
| `lumen_media_bytes_total` | `capability, model, provider, media_type` | Decoded media bytes processed (M9). |
| `lumen_http_request_duration_seconds` | `method, path, status` | Wall time of every HTTP request, including `/health` and `/metrics`. `path` is the matched route template, never the raw URI. Streaming responses count time-to-response-headers. |
| `lumen_request_duration_seconds` | `capability, model, provider, status` | End-to-end latency of one accounted API call. For streaming chat this covers the full stream, recorded when accounting closes. |
| `lumen_circuit_state` | `provider, model` | Circuit-breaker state: `0` closed, `1` open, `2` half-open. |
| `lumen_provider_up` | `provider` | Background health-probe result: `1` up, `0` down (absent = unknown / not probed). |
| `lumen_usage_log_dropped_total` | none | Usage-log entries dropped because the logging channel was full. |
| `lumen_metadata_rejected_total` | none | `x-lumen-metadata` headers dropped as malformed or out of bounds. |
| `lumen_config_reloads_total` | none | Successful configuration hot reloads. |
| `lumen_config_reload_failures_total` | none | Configuration reloads rejected as invalid; the previous config kept serving. |

`lumen_tokens_total`, `lumen_rerank_search_units_total`, `lumen_media_total`
and `lumen_media_bytes_total` also gain one extra label per key listed in
`telemetry.metadata_labels` - see
[Usage log & multi-tenant metadata](usage-log.md) for the allowlist
mechanics and a multi-tenant query example.

Per-request **cost** is not a Prometheus series: it is a `usage_log`-only
figure, computed from the `cost_per_1m_*` / `cost_per_1k_searches` prices in
config. See [Token accounting & cost](token-accounting.md).

## See it live: the monitoring rig

`monitoring/` is a one-command Docker Compose stack that runs the gateway
against real providers with Prometheus scraping it and a pre-provisioned
Grafana dashboard covering every metric above: token rates by
provider/model/capability/direction, rerank search units, media accounting,
circuit-breaker state and the internal counters. `./smoke.py` exercises
chat, streaming, embeddings and rerank per provider and asserts a non-zero
token count on each; `./traffic.py` generates sustained randomized traffic,
tagged with multi-tenant metadata, so the dashboard has something to show.
It is the fastest way to see all of this live - see `monitoring/README.md`.
