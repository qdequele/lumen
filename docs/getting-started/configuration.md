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

## Applying a new config over the admin API

**`PUT /admin/config` is the highest-privilege route in the gateway: it can
repoint any provider's `base_url` (or add a new provider entirely) and
thereby redirect customer traffic to a different upstream.** Master key
required, same as every other `/admin/*` route.

The `If-Match` contract:

- Send the submitted document as the raw request body (`Content-Type` is
  irrelevant; the body is treated as the TOML file's new contents).
- Send the `hash` from a prior `GET /admin/config` as the `If-Match` header.
  A missing `If-Match` header is rejected with `400` (`LM-1001`) - the
  request is malformed like any other missing-required-input case.
- If `If-Match` does not equal the config file's *current* hash (someone
  else applied a change since you last read it), the request is rejected
  with `412` (`LM-1004`) and the file is left untouched. `GET /admin/config`
  again to see what changed, then re-apply against the fresh hash.

What happens on a successful apply:

1. The submitted bytes are staged in a temporary file next to the real one
   (same directory, so the final rename is atomic).
2. The staged file is validated exactly like a hot reload would: parsed,
   merged with `LUMEN_*` env vars, and used to build a candidate provider
   registry. A parse failure or a registry-build failure (e.g. a provider
   missing a required `base_url`) is rejected with `400` (`LM-1001`); the
   staging file is removed and the real config file is never touched. The
   error message names the offending field or setting (e.g.
   `"server.first_token_timeout_ms must not be 0"`) but never the staging
   file's own filesystem path - that path is an implementation detail of
   this route, not something the operator wrote or needs to see, and it is
   logged server-side instead.
3. Only once validation succeeds is the *current* file copied to a `.bak`
   sibling (e.g. `lumen.toml.bak`) - one generation of history, enough to
   revert a bad apply by hand - and the staged file renamed into place.
4. The hot-reload trigger fires, so the new config takes effect without a
   restart (see [Hot reload](#hot-reload)).

A rejected apply (`400` or `412`) is guaranteed to leave the config file
byte-for-byte unchanged and to leave no temporary file behind. Concurrent
`PUT`s are serialised gateway-side, so two operators racing the same
pre-apply hash can never both land: exactly one wins, and the other sees a
`412` for a hash that moved out from under it, never a silently corrupted
mix of the two documents.

## Validate before you boot

Run `lumen --check-config --config config.toml` to validate a config file
without starting the server. See [Installation](installation.md).
