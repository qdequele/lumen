# Configuration basics

Everything is one TOML file, plus `LUMEN_*` environment variable overrides
that use `__` for nesting, e.g. `LUMEN_SERVER__PORT=9090`. In the TOML file
itself, top-level keys must appear before any `[table]` header. The
exhaustively commented reference is
[`config.example.toml`](https://github.com/qdequele/lumen/blob/main/config.example.toml)
on GitHub.

## Section tour

**`log_format`** - `"pretty"` (human-readable, default) or `"json"`
(production).

**`[server]`** - the HTTP server: `host` (bind address; use `"0.0.0.0"` in a
container) and `port` (must not be 0), `body_limit` (max request body in
bytes), `first_token_timeout_ms` (how long to wait for the upstream's first
sign of life before failing with `LM-3011`; for streaming this is time to the
first SSE frame, for non-streaming it is the whole upstream call), and
`sse_heartbeat_ms` (idle interval after which a `: ping` SSE comment keeps
proxies from reaping a silent stream).

**`[auth]`** - virtual keys, hard budgets, quotas and the usage log.
Disabled by default, in which case the gateway is an open proxy with no
database at all. See [Keys & budgets](../operations/keys-budgets.md).

**`[telemetry]`** - which `x-lumen-metadata` keys become Prometheus labels
on the token counters. See [Usage log](../operations/usage-log.md).

**`[resilience]`** - retries, fallbacks, circuit breaker, timeouts and
health checks. Every value is the built-in default and the whole section is
optional; see `config.example.toml` for the full set. Details in
[Resilience](../operations/resilience.md).

**`[[providers]]` / `[[providers.models]]`** - one `[[providers]]` block per
upstream (`id`, `upstream_id`, `capabilities`, `modalities`, costs,
per-model `fallbacks`). See [Providers](../providers.md) for the full
provider matrix and per-provider notes.

**`[image_fetch]`** - server-side fetching of remote image URLs for
multimodal input. See [Multimodal input](../embeddings/multimodal.md).

## API keys

API keys are never written in the config. A provider references the *name*
of the environment variable that holds its key, via `api_key_env`.

## Hot reload

A `SIGHUP`, a file watch, or an admin provider-key rotation triggers a
reload: the new config is validated, then the provider registry, price
table, resilience policy and the runtime-safe `[auth]` knobs are atomically
swapped (the bind address and a few other knobs still need a restart).
Details in [Deployment](../operations/deployment.md#hot-reload).

## Viewing the live config over the admin API

`GET /admin/config` (master key required) returns the config file the
gateway booted from, verbatim:

```json
{ "config": "<raw toml, byte for byte>", "hash": "<64 hex chars, BLAKE3>" }
```

`config` is the file's exact bytes, never a re-serialisation of the merged
in-memory config: `Config::load` overlays `LUMEN_*` environment variables on
top of the file, and showing that merged view would make environment
overrides look like file content, and a later write would bake them in
permanently. `hash` is a BLAKE3 content hash of those same bytes, meant to be
echoed as `If-Match` on the `PUT /admin/config` that applies a new one (ADR
010), so two operators editing at once cannot silently clobber each other.

## Validate before you boot

Run `lumen --check-config --config config.toml` to validate a config file
without starting the server. See [Installation](installation.md).
