# Changelog

All notable changes to LUMEN are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Route-not-found error envelope** (issue #88). Requests matching no route
  (trailing-slash, extra-segment and other near-miss paths) used to return a
  bare, empty-body 404 outside the LM envelope, and bypassed the virtual-key
  auth layer. A `Router::fallback` now answers every such miss with the new
  stable `LM-1003` route-not-found code (404, `type: invalid_request`),
  documented in `docs/errors.md`. The fallback sits outside the auth layer on
  purpose: an unmatched path answers 404 even when auth is enabled, which
  leaks no more than the bare 404 it replaces (it names no path and discloses
  no route), while a matched `/v1` route without a key still returns 401
  `LM-4004`. `/health` latency isolation and `/metrics` are unchanged.
- **Virtual key deletion and rotation** (issue #66). `DELETE /admin/keys/{id}`
  soft-deletes a key: the row is tombstoned (`deleted_at` column, migration
  `0005_key_deleted_at.sql`) rather than removed, so `usage_log.key_id`
  attribution and audit history survive. A deleted key stops authenticating
  immediately (the live in-memory table is updated, like PATCH), is hidden
  from `GET /admin/keys` unless `?include_deleted=true`, and rejects further
  PATCH/DELETE/rotate with 400 `LM-1001`. `POST /admin/keys/{id}/rotate`
  mints a new secret through the same generation path as creation and
  returns it in the same one-time response shape; the id, name, budgets,
  accrued spend and quota windows are all preserved (the live entry is kept,
  only its hash alias swaps), so the old plaintext dies on the spot and the
  new one works without a restart.
- **`lumen keys create` / `lumen keys list`: offline virtual-key bootstrap**
  (issue #68). Creating the first virtual key no longer requires starting the
  server and crafting an authenticated curl against `POST /admin/keys`: the
  new `keys` subcommand opens the SQLite store at `auth.db_path` directly,
  gated by the same `LUMEN_MASTER_KEY` env var as the /admin API, and prints
  the record plus the one-time plaintext (stdout only, never logged) in the
  same JSON shape as the admin route. `keys list` prints the records (no
  hashes, no plaintext). Non-finite (`NaN`/`inf`) and negative budget/limit
  values are refused at parse time (a `NaN` budget would silently mint an
  UNLIMITED key), every value flag accepts both `--flag value` and
  `--flag=value`, and the hot-reload path now re-syncs the in-memory
  virtual-key table from the DB so a CLI-created key becomes live on the next
  reload with no restart (in-memory spend is preserved). Documented in
  `docs/operations/keys-budgets.md`, and the quickstart now links the
  bootstrap step.
- **`GET /admin/usage`: usage and spend reporting over HTTP** (issue #64).
  The `usage_log` table finally has a query surface: a master-key-gated
  admin route returning aggregates per group - request counts split by
  status class, token totals, the estimated-vs-upstream split (ADR 003),
  rerank search units, media counts and cost. Filters: `key_id`, `model`,
  `provider`, `capability` and an inclusive `since`/`until` window (unix
  seconds or RFC3339); `group_by` picks the dimension (`model` default,
  `model_used`, `provider`, `capability`, `key_id`, `status`, `total`) and
  `limit` (default 100, max 1000) bounds the result, with `truncated`
  flagging cutoffs (most expensive groups win). Defaults: last 24 hours,
  grouped by model. Invalid parameters are 400 `LM-1001`. The read runs
  directly against SQLite - admin-only, off the hot path - and rows reach
  it through the batched writer, so the last flush interval may lag. New
  `usage_log.provider` column (migration 0004) records the provider that
  actually served each request (admission refusals carry the requested
  model's primary provider); pre-existing rows report an empty provider.
  Rolling back to a pre-0004 binary only requires clearing the migration
  ledger (`DELETE FROM _sqlx_migrations WHERE version = 4`); the extra
  column itself is harmless to older binaries.
- **`GET /v1/models/{id}` (OpenAI retrieve-model)** (issue #67): returns the
  same per-model object as the corresponding `GET /v1/models` list entry (id,
  `object: "model"`, `owned_by`, `capabilities`, `modalities`), served from the
  same registry snapshot as the list. An unknown id is a 404 with the standard
  `LM-2001` envelope, and the route sits behind the same virtual-key auth layer
  as every other `/v1` route. OpenAI SDK calls like `client.models.retrieve(...)`
  now work against LUMEN.
- **Chat support for the native `ollama` kind** (issue #63). A model declaring
  `capabilities = ["chat"]` on an `ollama` provider now resolves and serves
  `POST /v1/chat/completions`, streaming and non-streaming. Chat reuses the
  shared OpenAI-compatible implementation pointed at `{base_url}/v1` (Ollama's
  OpenAI-compatible surface on the same server root as the native `/api/embed`
  path), so zero-copy SSE passthrough (ADR 004), cancellation on client
  disconnect, and ADR 003 token accounting (upstream usage when present, else
  a local count marked `estimated`) all apply. The `/api/version` health probe
  and the embed path are unchanged. Documented in `docs/providers.md` and
  `config.example.toml`.
- **First-class `api_version` config field for the `azure` kind** (issue
  #65). The Azure OpenAI API version no longer has to be smuggled into
  `base_url` as an `?api-version=...` query string: providers of
  `kind = "azure"` accept an optional `api_version` field, threaded through
  `ProviderSpec` into the provider. Precedence: the explicit `api_version`
  field wins over an `?api-version=...` query string on `base_url` (which
  keeps working for back-compat), which wins over the pinned built-in
  default. Setting `api_version` on any other kind is a boot-time config
  validation error.

### Fixed

- **Cohere chat silently dropped image parts (issue #73).** The v2 chat
  translator flattened every message to plain text via `MessageContent::text()`,
  so a correctly-declared Command-A-Vision model (`modalities = ["text",
  "image"]` on the `cohere` kind) answered as if the user sent text only.
  Messages carrying image parts are now translated to Cohere v2 content
  blocks (`{"type":"text",...}` / `{"type":"image_url","image_url":{...}}`),
  order preserved, `detail` forwarded untouched; text-only messages keep the
  plain-string fast path. Cohere fetches remote `http(s)` URLs itself, so
  both URL forms pass through and the default `accepts_remote_image_url`
  (true) stands; provider-native references (`anthropic-file:`, `gs://`,
  Gemini Files API URIs) stay honest `LM-2008` pre-flight rejections.
  Upstream usage remains authoritative for vision requests, with the
  ADR 003 per-image estimation fallback when the upstream reports none.
  Covered by translator unit tests plus wiremock conformance tests at the
  provider and gateway levels (wire-shape, remote-URL pass-through, LM-2008
  verdicts, cancellation).
- **A model declaring `embed` on `groq`, `deepseek`, `openrouter`,
  `perplexity` or `xai` is now rejected at config load** (issue #74; new
  `RegistryError::NoUpstreamEmbeddings`, surfaced by boot, `--check-config`
  and hot reload) instead of building silently and 404ing on the first
  `/v1/embeddings` request. These hosted OpenAI-compatible kinds share the
  OpenAI embed wiring but expose no upstream `/embeddings` endpoint, so the
  failure was certain. Backed by a new `ProviderKind::supports_embeddings()`
  capability table; `fireworks`, `together`, `deepinfra`, `huggingface`,
  `cloudflare` and `vllm` embed models keep working. The check only applies
  when the provider uses the kind's default base URL: a custom `base_url`
  (an operator-run proxy that may serve embeddings) bypasses it, and
  `kind = "openai"` with a `base_url` override works as before.
  `docs/providers.md` capability table updated.
- **Gemini and Vertex AI embeddings** (issue #62). The `google` kind now
  serves `/v1/embeddings` through `models/{model}:batchEmbedContents` (batch
  limit 100, `dimensions` mapped to `outputDimensionality`), and `vertex_ai`
  through the regional, OAuth-authenticated `:predict` endpoint
  (`instances[].content`, one input per upstream call because
  `gemini-embedding-001` takes a single instance; the gateway fans batches out
  concurrently). Both kinds are text-only: pre-tokenized token-id arrays and
  image content parts are rejected with an honest `LM-1001` before any
  upstream call. Token accounting follows ADR 003: Gemini's
  `usageMetadata.promptTokenCount` and Vertex's summed
  `statistics.token_count` are reported as upstream usage when present,
  otherwise the gateway's local estimate (marked `estimated`) applies. Both
  kinds pass the shared embed conformance suite, including cancellation.

### Changed

- **Refreshed the LUMEN-vs-LiteLLM baseline** (`bench/run.sh` at commit
  `51fc809`): new committed run `bench/results/20260715T231135Z/` and updated
  numbers in `docs/perf-baseline.md`. Same relative story as the previous
  baseline: ~2.5 ms added p50 over direct-to-mock for LUMEN vs ~317 ms for
  LiteLLM, ~25x LiteLLM's throughput at ~7.6 MB vs ~1.03 GB RAM under load.

### Fixed

- **`response_format`, `seed`, `logprobs` and `parallel_tool_calls` were
  silently dropped by the translated chat providers** (issue #72). These
  fields pass through verbatim on OpenAI-compatible kinds but the translated
  kinds (anthropic, google, vertex_ai, bedrock, cohere) rebuilt the upstream
  request without them, so e.g. JSON mode on a Gemini-routed model silently
  returned unconstrained text. Now mapped natively where the upstream supports
  it: Gemini gets `generationConfig.responseMimeType`/`responseSchema` (with
  JSON Schema keys outside Gemini's OpenAPI subset stripped) and
  `generationConfig.seed`; Cohere v2 gets `response_format` (OpenAI's
  `json_schema` type collapsed onto Cohere's `json_object` + `json_schema`)
  and `seed`; Anthropic maps `parallel_tool_calls: false` to
  `tool_choice.disable_parallel_tool_use`. Genuinely unsupported field+kind
  combos now honor the per-provider `strict` flag (the issue #25 pattern):
  strict rejects with an honest 400 (`LM-1001`) naming the field and provider
  before any upstream call, lenient (default) drops with a `debug` log.
  `top_logprobs` and `logit_bias` get the same strict/lenient treatment on
  all four translated kinds. Per-provider matrix documented in
  `docs/providers.md`.
- **`LUMEN_MASTER_KEY` was folded into the config and broke `--check-config`
  and every real auth-enabled boot.** `Config::load` merged all `LUMEN_*`
  environment variables into the config via `figment::providers::Env`, so
  setting `LUMEN_MASTER_KEY` (the secret `boot_auth_stack` reads directly
  from the process environment, never a config value) produced a top-level
  `master_key` key that `Config`'s `#[serde(deny_unknown_fields)]` rejected
  with "unknown field: found `master_key`". This was discovered while
  documenting examples: `--check-config` failed whenever the var was set,
  and since `main()` calls `Config::load` before `boot_auth_stack` on every
  boot, any real deployment with `[auth] enabled = true` (which requires
  `LUMEN_MASTER_KEY`) hit the exact same failure and could not start.
  `Config::load` now excludes `master_key` from the `LUMEN_*` env merge
  (`Env::prefixed("LUMEN_").ignore(&["master_key"])`); the secret is still
  read normally by `boot_auth_stack` via `std::env::var`.

### Added - Test & benchmark debt (issue #27)

- **Direct fuzzing of the Anthropic/Google translation paths.** Each provider
  module gets a `#[cfg(fuzzing)] pub mod fuzzing` shim (compiled only under
  `cargo fuzz`, which sets `--cfg fuzzing` across the dependency graph, so
  normal builds are unaffected) exposing `translate_request`/
  `translate_response`. Four new `fuzz/fuzz_targets/` binaries
  (`anthropic_translate_request`, `anthropic_translate_response`,
  `google_translate_request`, `google_translate_response`), wired into the
  weekly fuzz CI matrix; each ran 10 000 libFuzzer iterations locally with no
  crashes. See `fuzz/README.md`.
- **Reproducible, committed LUMEN-vs-LiteLLM benchmark baseline.** `bench/run.sh`
  drives the full head-to-head (build/start the pinned stack, run k6 against
  direct/lumen/litellm, sample RAM under load, tear down) in one command and
  writes a timestamped report to `bench/results/`. `bench/compose.yaml` now
  pins LiteLLM and mockserver by tag *and* digest. A recorded baseline run is
  committed and linked from `docs/perf-baseline.md`, with an honest caveat
  about the recording host's noise.
- **Real-signal SIGTERM/SIGINT integration test.** `crates/server/tests/signal_shutdown.rs`
  (`#[cfg(unix)]`) spawns the actual `lumen` binary and sends it a genuine
  `SIGTERM`/`SIGINT` via `libc::kill`, asserting an in-flight request still
  completes and the process exits 0 - the real `tokio::signal` path, not just
  the injected-oneshot `serve()` tests.

### Fixed - Test determinism

- `monitoring/smoke.py` now exercises the Gemini tool-calling roundtrip
  (`check_chat_tools` added to the `google (gemini)` provider block, skipped
  like the others when `GEMINI_API_KEY` is absent) and its module docstring
  and `monitoring/README.md` no longer claim Gemini tool calls are
  unexercised, now that Gemini tool calling has shipped (issue #4).
- `resilience.rs::health_stays_fast_under_upstream_429_storm` no longer panics
  on a client-side TCP connect reset/broken-pipe under its 500-concurrent-request
  storm (a saturated OS accept backlog on some hosts, not gateway behaviour):
  the shared `post_chat` test helper now retries pre-response transport errors
  with backoff. Storm size is also overridable via
  `LUMEN_RESILIENCE_STORM_SIZE` (default unchanged: 500).
- `providers::embeddings::mistral_passes_embed_conformance_suite`'s
  cancellation scenario widened its mocked-delay/elapsed-bound margin (2s/1s →
  3s/2s) for headroom under full workspace-test parallelism, without weakening
  what it proves.

### Changed - First-frame-peek streaming retry (issue #7)

- **Streaming commitment now happens at the first content frame, not at the
  open.** After the upstream stream opens (2xx + headers), the gateway peeks the
  first frame before committing the response. An upstream that opens `200` then
  errors, or closes, before delivering any content frame is now a *pre-commit*
  failure: it retries and falls over per the existing resilience policy (and
  penalises the provider's circuit breaker) instead of sending the client a
  terminal SSE error frame. Once the first content frame arrives the request is
  committed and the M4 frame guards own the rest, unchanged (a mid-stream error
  is still a terminal error frame, never retried).
- The peek buffers at most one frame (zero-copy, ADR 004), is bounded by the
  per-attempt `first_token` timeout (a silent upstream fails over), and aborts
  the upstream on a client disconnect during the peek window (no fallback).
- New `ProviderError::EmptyStream` (retryable, provider-fault) models an upstream
  that opened but sent no content frame; it maps to the existing `LM-3010` when
  every link in the chain empty-streams. See ADR 005, 2026-07-15 amendment.

### Added - Opt-in accurate per-model tokenizer (ADR 003)

- New `[tokenizer]` config section with `mode = "heuristic"` (default) or
  `"accurate"`. The default is unchanged: the cheap byte heuristic, zero added
  latency, allocation-light, hot-path-safe. `"accurate"` opts into exact
  per-model BPE counting (`tiktoken-rs`) for the local estimation fallback.
- Accurate mode selects the tiktoken vocabulary by model prefix: `cl100k_base`
  for `gpt-4` / `gpt-3.5` / `text-embedding-3`, `o200k_base` for `gpt-4o` /
  `o1` / `o3` / `gpt-4.1` / `gpt-5`. Non-OpenAI models (Claude, Mistral,
  Llama, ...) keep the byte heuristic, as does any tokenizer failure -
  counting never fails or rejects a request.
- The request path is never delayed: the response envelope always carries the
  inline heuristic estimate (flagged `estimated`), and the accurate BPE count
  is computed AFTER the response is handed off, by a spawned background
  accounting task running the BPE pass on the blocking pool via
  `tokio::task::spawn_blocking` (repo rule 2). The accurate number lands in
  Prometheus (`lumen_tokens_total`) and `usage_log` - the accounting surfaces.
  Latency histograms and `usage_log.latency_ms` are frozen at response time,
  so the deferral never inflates them. Refinement fires only when an upstream
  reported no usage (chat non-streaming, embeddings, rerank). Encoders are
  built once at startup, never on the request path. Local counts stay flagged
  `estimated = true`; upstream-reported usage always wins. Streaming input
  counts remain heuristic (the stream is not buffered).
- New dependency `tiktoken-rs` (pure Rust, MIT; pulls no OpenSSL, honoring the
  rustls-only mandate).

### Added - Per-provider connect timeout (issue #24)

- A provider block may now set `connect_timeout_ms`, joining the existing
  per-provider `first_token_timeout_ms` and `total_timeout_ms` overrides. When
  set, that provider is given its own `reqwest::Client` (built once at registry
  construction) with the override as its connect timeout and the same overall
  backstop as the shared client. Providers that do not override keep sharing the
  one pooled client, so cross-provider connection pooling is preserved for the
  common case. Trade-off: an overriding provider no longer shares the pool (its
  connections pool only within its own client). Config validation rejects a
  `connect_timeout_ms` of `0`. The override is picked up on hot reload like the
  other two, because the registry rebuilds its clients from the new specs.
  Supersedes the ADR 005 deferral of per-provider connect timeouts (amended
  2026-07-15).

### Added - Hot reload extended to auth knobs + DB provider-key rotation

- **Hot reload now retunes the safe `[auth]` knobs without a restart.** A
  `SIGHUP`, config-file change, or admin trigger swaps the budget-flush cadence
  (`auth.flush_interval_ms`) and the usage-log retention window
  (`auth.retention_days`) into a shared cell the flush/purge background tasks
  read live on their next tick. The bounded usage-log channel knobs
  (`usage_channel_capacity`, `usage_batch_max`, `usage_flush_ms`),
  `auth.db_path`, `auth.enabled` and the server bind address stay boot-time
  (rebinding a live listener is out of scope); this is documented in the
  `reload` module and `docs/backlog.md`.
- **Rotating a DB-stored provider key takes effect without a restart.**
  `PUT /admin/provider-keys/{name}` now pings the hot-reload trigger after
  sealing the key; the reloader re-reads provider keys from the encrypted store
  (off the request path, in the reload task), rebuilds the provider registry and
  swaps it atomically. Every reload (SIGHUP / file change / trigger) re-reads the
  DB keys, so a rotation is picked up even without the admin trigger. A DB read
  error keeps the previous snapshot, so a reload never strips a working key.
  Environment-sourced keys keep precedence (rotation only affects env-keyless
  providers). Closes the M5/M7 backlog debt "DB-stored provider keys are
  boot-time only" and "auth knobs still boot-time".

### Changed - Rate-limit and usage-log accounting refinements (issue #26, ADR 007)

- **TPM is now settled to real usage, like the budget.** A successful request
  debits the pre-call token estimate to the per-minute window and then adjusts
  it to the real token count when accounting closes. A large `max_tokens`
  reservation no longer starves a key for a full minute after a short reply.
  A dropped (failed/cancelled) reservation deliberately keeps the TPM debit -
  a request that hit the gateway still counts against the rate limit - while
  the budget reservation is refunded (no money was spent).
- **A request refused inside admission no longer burns quota.** When TPM
  refuses after RPM was counted, or the budget refuses after RPM and TPM were
  counted, the earlier bumps are rolled back, so a rejected request consumes no
  RPM/TPM.
- **Rejected requests now produce a usage-log row.** An admission refusal
  (402/429) writes a status-only `usage_log` entry (zero tokens, zero cost, the
  `status` column carrying the rejection) via the same bounded, non-blocking
  channel as successful requests - never a synchronous DB write on the request
  path. Enables per-key rejection analytics. (401 stays unlogged: it is refused
  in the auth middleware before accounting opens.)
- **`usage_log.metadata` keeps JSON value types.** Metadata values are stored
  as typed JSON (`{"batch":42,"canary":true}`) instead of stringified
  (`{"batch":"42"}`), so numeric/boolean filtering via SQLite `json_extract`
  works. Prometheus labels still stringify (labels are always strings). The
  column stays TEXT - no migration.

### Added - Richer per-kind health probes (#23)

- The background health-check task (`resilience.health_check_enabled`) now
  uses a real liveness endpoint for every self-hosted, keyless provider kind,
  not just TEI: **vLLM** (server-root `GET /health`; a trailing `/v1` in the
  configured `base_url` is stripped, since the documented convention is
  `base_url = "http://host:8000/v1"`) and **Ollama**
  (`GET {base_url}/api/version`), both built for this exact use. A non-2xx
  response on these now correctly marks the provider `down`, same as TEI.
- Keyed vendor kinds (OpenAI, Anthropic, the OpenAI-compatible hosts, ...)
  keep the bare `base_url` reachability probe (any HTTP response counts as
  `up`) - an unauthenticated call to a real endpoint like `/models` would 401
  a healthy server, which is a worse signal, and the probe task does not carry
  provider API keys. This is the explicit default match arm in
  `ProbeTarget::probe_url`/`is_liveness_endpoint`, so any new `ProviderKind`
  (several are landing in parallel PRs) automatically inherits bare
  reachability until it earns a real probe.

### Added - AWS Bedrock provider (SigV4, Converse API)

- **New `bedrock` provider kind: chat via the AWS Bedrock Converse API.** One
  uniform schema (`POST /model/{modelId}/converse` and `/converse-stream`)
  covers the Anthropic, Meta Llama, Amazon Titan/Nova, Mistral and Cohere model
  families, so the legacy per-model `InvokeModel` schemas are intentionally not
  implemented. Bidirectional OpenAI ⇄ Converse translation including system
  prompts, `inferenceConfig`, tools, images (inline `data:` URIs) and usage
  (`inputTokens`/`outputTokens`, mapped per ADR 003).
- **AWS Signature Version 4 request signing**, hand-rolled over `hmac` + `sha2`
  (pure-Rust, rustls-compatible, no OpenSSL and no AWS SDK runtime) to keep the
  dependency and RAM footprint aligned with the project pillars. The canonical
  request path is double-encoded per the non-S3 SigV4 rule (wire `%3A`, signed
  `%253A`), so versioned model ids containing `:` sign correctly; verified by
  known-answer tests against AWS's published `aws-sig-v4-test-suite` vectors
  (`get-vanilla`, `post-vanilla`) plus a double-encoding KAT. Credentials are
  re-read from the standard AWS environment variables (`AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`) on every request, so
  rotated values apply without a restart; only static keys and pre-issued STS
  tokens are supported (no IMDS/SSO chain). The secret and session token are
  never logged and never appear in `Debug`. The signing region is resolved from
  the `bedrock-runtime.{region}` endpoint host (standard and VPC/PrivateLink
  shapes) or from `AWS_REGION` / `AWS_DEFAULT_REGION`; an undeterminable region
  is a startup error, never a silent default.
- **Streaming** parses the AWS event-stream binary framing
  (`vnd.amazon.eventstream`: prelude, headers, payload, CRCs) into OpenAI chunks;
  CRC validation is skipped (TLS assures integrity) but frame lengths are checked
  exactly. Cancellation aborts the in-flight upstream request like every other
  provider. Wiremock tests cover signed-header well-formedness, the Converse
  round-trip, event-stream frame parsing from byte fixtures, streaming
  translation, partial-stream (no fabricated `[DONE]`), cancellation, and
  secret hygiene.

### Added - Additional rerank providers (Mixedbread, Pinecone, NVIDIA NIM, Together)

- Four new rerank kinds broaden first-class rerank coverage (issue #19):
  - **`mixedbread`** (`mxbai-rerank-*`, hosted): bearer auth,
    `POST /v1/reranking` (note the path: `reranking`, not `rerank`) with the
    renamed request fields `input`/`top_k` and a `data`-nested response
    (`results`/`relevance_score` also accepted). Token-billed, so the gateway
    reports an ADR-003 `estimated` token count.
  - **`pinecone`** (hosted inference): authenticates with the `Api-Key` header
    (not a bearer token) plus a pinned `X-Pinecone-API-Version`, sends
    documents as `{ "text": ... }` objects, and carries the upstream
    `usage.rerank_units` through as `search_units` (not estimated).
  - **`nvidia`** (NIM ranking): `base_url` required (the NIM root), key
    optional (self-hosted NIMs are keyless). Posts to `{base}/v1/ranking` with
    `query: { text }` / `passages: [{ text }]`; NIM returns a raw **logit**,
    passed through unchanged as `relevance_score` (unbounded, comparable only
    within one response, no sigmoid applied). No `top_n` on the wire, so the
    gateway truncates afterwards (as for TEI).
  - **`together`**: the existing OpenAI-compatible `together` kind now also
    serves **rerank** (LlamaRank) natively via its Cohere-shaped `/rerank`
    endpoint, mirroring how `cloudflare` adds native rerank; one provider entry
    serves chat, embed and rerank against the same `base_url` and key.
- All four honour `CancellationToken` (a client disconnect aborts the upstream
  call), map upstream status codes to `ProviderError` with a retry hint, and
  keep the API key out of `Debug`/logs. Covered by the shared rerank
  conformance suite plus dedicated request-shape/auth-header tests.

### Added - Cohere chat (Command R / R+)

- The `cohere` provider kind now implements `ChatProvider` alongside its
  existing embed and rerank capabilities: `POST /v2/chat`, non-streaming and
  streaming (SSE). Cohere's v2 wire shape is OpenAI-adjacent (roles live
  directly in `messages`, unlike Anthropic's top-level `system` hoist; an
  assistant's `tool_calls` are already OpenAI-shaped), so translation is
  mostly a matter of field renames (`top_p` -> `p`, `stop` -> `stop_sequences`)
  and `tool_choice` collapsing to Cohere's `REQUIRED`/`NONE` strings (forcing
  one named tool has no v2 equivalent and falls back to `auto`, dropped with a
  `debug` trace). `n` (multiple completions) has no v2 equivalent and is
  likewise dropped with a `debug` trace rather than silently ignored.
- Streaming translates Cohere's typed SSE events (`message-start`,
  `content-delta`, `tool-call-start/-delta`, `message-end`) to OpenAI chunks
  event by event, bounded state only (mirrors the Anthropic/Google streaming
  translators) - `message-end` is Cohere's sole terminal event and carries
  both the finish reason and full usage, so it emits the final chunk followed
  immediately by the stream terminator.
- Token usage (ADR 003): `usage.tokens` (actual pre-billing counts) is
  preferred over `usage.billed_units` (what's charged, which can differ e.g.
  under caching discounts); a response reporting neither leaves `usage: None`
  so the gateway's local estimator fills in an honestly-flagged count.
- Every trait method honours `CancellationToken`, including aborting the
  upstream connection on stream drop (client disconnect), matching every
  other provider.

### Fixed - Deterministic 429-storm resilience test

- `health_stays_fast_under_upstream_429_storm` no longer flakes on macOS. The
  kernel clamps every listen backlog to `kern.ipc.somaxconn` (128 by default)
  and answers accept-queue overflow with RST, so the test's 500 simultaneous
  loopback connects were reset by the OS before the gateway ever saw them
  (surfacing as connect `ECONNRESET` or first-write `EPIPE` panics). The storm
  now retries kernel-level connection failures with a bounded budget (matching
  Linux SYN-retransmit semantics), shares one pooled client with a timeout so a
  wedged gateway fails loudly instead of hanging, pre-warms the health probe's
  keep-alive connection so the latency bound measures the handler path rather
  than the kernel's accept queue, and raises the process soft fd limit (the
  storm holds ~2000 sockets across four hops in one process, above launchd's
  256 default). What the test asserts is unchanged: /health stays under the
  same bound during the storm and all 500 requests complete with 429/503.

### Added - Google Vertex AI provider

- New provider `kind = "vertex_ai"`: Gemini models on Google Cloud Vertex AI
  (chat, streaming and non-streaming). Distinct from `kind = "google"` (the
  public Gemini Developer API): requests go to the regional endpoint
  `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:generateContent`
  (`:streamGenerateContent?alt=sse` when streaming), reusing the existing
  Gemini wire translation.
- GCP service-account OAuth: LUMEN signs an RS256 JWT assertion with the
  account's private key and exchanges it for a short-lived `Bearer` access
  token (scope `cloud-platform`). Tokens are cached in memory and refreshed
  60 s before expiry, keeping the exchange off the per-request hot path;
  concurrent refreshes coalesce onto one fetch. A token-exchange failure is a
  provider-named upstream error (LM-3003 family), never a misleading client
  401. The private key is redacted from all `Debug` output, logs and errors.
- Config: `api_key_env` names an env var holding the full service-account key
  JSON; `base_url` carries the GCP region (e.g. `us-central1`); the project id
  comes from the credentials. An unset credentials env var still boots (parity
  with other providers) and fails per request; garbage credentials JSON is a
  startup error naming the provider.
- New workspace dependency `jsonwebtoken` (RS256 signing; `ring`-backed, no
  OpenSSL, consistent with the rustls-only policy).

### Added - Azure OpenAI provider (deployment routing + api-version)

- New `azure` provider kind (chat + embeddings): reuses the OpenAI JSON
  wire schema verbatim (near-passthrough, like `mistral`), with the three
  Azure-specific deltas bridged in `crates/providers/src/azure`:
  - URL construction is deployment-routed:
    `{endpoint}/openai/deployments/{deployment}/{chat/completions|embeddings}?api-version=...`,
    not the generic OpenAI-compatible `base_url`-swap path.
  - Auth is the `api-key` header, never a bearer token.
  - Deployment routing reuses the existing `upstream_id` aliasing mechanism -
    set a model's `upstream_id` to the Azure deployment name, no new config
    field needed (the router already rewrites `req.model` to `upstream_id`
    before calling the provider).
  - `api-version` is selected via a `?api-version=YYYY-MM-DD` query string on
    `base_url`, defaulting to a pinned recent version when omitted. There is
    no dedicated `api_version` config field yet - see the module doc comment
    and `docs/providers.md#azure` for the known gap and workaround.
- `base_url` is required for `azure` (every Azure resource endpoint is
  operator-specific; there is no shared public default).
- `config.example.toml` and `docs/providers.md` updated with a worked example.
### Changed

- Documentation restructured around capabilities: the mdBook at
  https://qdequele.github.io/lumen/ is now the canonical documentation home
  (getting started, chat / embeddings / reranking guides, operations incl.
  analytics and budgets, examples); the README slimmed down accordingly.
  Added runnable `examples/` scenarios validated in CI by `--check-config`.

### Added - Provider-native file/GCS image URI sources (issue #12)

- Chat vision content parts now accept two provider-native image references
  alongside inline `data:` URIs and remote `http(s)` URLs: an Anthropic Files
  API reference, spelled `anthropic-file:<file_id>` in the `url` field, and a
  Gemini-native file/GCS reference (`gs://bucket/object`, or a Gemini Files
  API URI under `https://generativelanguage.googleapis.com/`).
- Anthropic translates `anthropic-file:<file_id>` to a `source: {type: "file",
  file_id}` content block instead of `base64`/`url`.
- Gemini translates a `gs://`/Files API URI to a `fileData.fileUri` part
  instead of `inlineData`; the mime type is included only when it can be
  confidently inferred from the URI's extension, otherwise omitted so Gemini
  falls back to the mime type it recorded at upload time. Caveat: the Gemini
  Developer API (the `google` kind's default endpoint) only resolves its own
  Files API URIs; `gs://` is a Vertex AI capability, forwarded verbatim and
  rejected by the default upstream (documented in `docs/providers.md`).
- A Gemini Files API URI is also an `https://` URL; it is exempt from the
  `LM-2004` remote-URL pre-flight so it reaches Gemini as `fileData.fileUri`
  instead of being wrongly rejected.
- Sending a provider-native reference to a provider that cannot resolve it
  (e.g. an Anthropic `file_id` routed to Gemini) is now rejected before any
  upstream call with a new `LM-2008` (400) client error, instead of surfacing
  as a translation failure (502) from the mismatched provider.

### Added - Per-image vision token heuristic for the estimation fallback

- The local prompt-token estimation fallback (`estimate_chat_prompt`, ADR 003)
  no longer counts an image content part as `0` tokens. Each image part now
  contributes a flat per-image estimate: `85` tokens for `"detail": "low"`
  (OpenAI's exact, resolution-independent low-detail cost) or `765` tokens for
  `"detail": "high"`/`"auto"`/unset (an approximation of OpenAI's tile formula
  for a typical ~1024x1024 image, since the gateway does not decode image
  bytes on the request path to learn the real dimensions - see the ADR 003
  vision addendum). This only affects requests where the upstream reports no
  `usage` at all; upstream-reported usage was already accurate and is
  untouched. Fixes #9.

### Added - Gemini tool calling

- **The Google (Gemini) provider now supports tool calling** instead of
  silently dropping it (issue #4). Request translation maps OpenAI `tools` to
  Gemini `tools[].functionDeclarations` and `tool_choice` to
  `toolConfig.functionCallingConfig` (`auto`/`required`/`none`/specific
  function). Assistant `tool_calls` become `functionCall` parts (role `model`)
  and role `tool` messages become `functionResponse` parts (role `user`,
  consecutive results merged). Both the non-streaming and streaming response
  translators surface Gemini `functionCall` parts as OpenAI `tool_calls` and
  map the trailing `STOP` to `finish_reason: "tool_calls"`. A synthetic call
  id (`call_<n>`) is minted since Gemini does not return one.

### Added - `--check-config` validation mode

- New `lumen --check-config [--config <PATH>]` mode for CI / deploy pipelines
  (issue #21): loads and fully validates the config the same way the server
  does at boot, including semantic validation and provider registry
  construction (which catches reference errors such as a missing `base_url`
  for a self-hosted provider). Prints a clear success or failure message and
  exits 0 when the config is valid, non-zero otherwise. Binds no listener,
  opens no database, and contacts no provider, so it is safe to run ahead of
  a real boot.
- New `lumen_server::check_config` library function backs the flag, kept
  separate from `main` so the validation logic stays unit-testable.

### Added - Embeddings/rerank input format gaps (issue #25)

- **Token-array embedding inputs.** `POST /v1/embeddings` now accepts
  pre-tokenized `input`: a single token-id array (`[1,2,3]`, one item) or a batch
  of them (`[[1,2],[3,4]]`). They pass through natively on OpenAI-compatible
  providers and count one token per id in the estimation fallback. String and
  string-batch inputs are unchanged (untagged order tries them first).
  Providers whose APIs only take text (Cohere, TEI, Ollama, Jina, Voyage,
  Mistral) reject token-array input with an honest 400 (`LM-1001`,
  `ProviderError::UnsupportedInput`) BEFORE any upstream call, instead of
  sending an empty or garbled body upstream (rule 8).
- **base64 embedding output.** When a client sets `encoding_format: "base64"`,
  each vector is re-encoded as OpenAI-style base64 (little-endian `f32` bytes) at
  the response edge. Because the gateway always holds vectors as `Vec<f32>`
  internally, this works for every provider, including Ollama and TEI that have
  no upstream `encoding_format`. Any other value serializes as a float array.
- **Rerank object documents with `rank_fields`.** `RerankDocument` object
  documents now keep all their fields, and the request accepts an optional
  Cohere-style `rank_fields` selector. Each object document is reduced to a
  single ranking text at the gateway edge (selected fields joined with newlines,
  or the `text` field when no selector), so providers still only ever see plain
  text. Note: with `return_documents: true`, an object document's echoed
  `document.text` is that reduced ranking text, not the original JSON object.
- **Ollama strict mode.** A new per-provider `strict = true` (config
  `[[providers]] strict`) makes Ollama reject a request that sets `dimensions`
  (which it cannot honor) with a 400 (`LM-1001`) naming the field, instead of
  silently returning full-width vectors. The default stays lenient (drops the
  field with a debug log). Backed by a new `ProviderError::UnsupportedField`
  that maps to a client 400 and is never retried or failed over.

### Added - Cohere embed `input_type` override (#22)

- `POST /v1/embeddings` now accepts an `input_type` extra field so a caller
  can override Cohere's query-vs-document intent (`search_query`,
  `search_document`, `classification`, `clustering`) instead of always
  getting the `search_document` default - materially affects retrieval
  quality for query-time embeddings. `EmbedRequest` gained an `extra` map
  (the `serde(flatten)` idiom `ChatRequest` already uses) that captures
  unknown request fields for provider translation code and survives automatic
  batching intact. Unlike the chat path, `extra` is never re-serialized into
  an outgoing provider body: only the Cohere translation consumes
  `input_type`, and unknown fields stop at the gateway rather than being
  forwarded to OpenAI-compatible upstreams (which may be strict). An
  unrecognized `input_type` is rejected with `LM-1001` before any upstream
  call. See `docs/providers.md` § cohere.

### Added - Token-based rerank usage for Jina/Voyage (issue #10)

- `RerankUsage` gains `total_tokens` and `tokens_estimated`, additive to the
  existing `search_units`/`estimated` pair. Jina and Voyage bill rerank in
  tokens rather than search units and report `usage.total_tokens`; the
  gateway now surfaces that upstream count unflagged (`tokens_estimated`
  omitted), instead of always synthesising a local estimate.
- Every rerank response now carries a `total_tokens` count for uniform
  observability (ADR 003): when the upstream does not report one (Cohere,
  TEI, or Jina/Voyage without `usage`), the gateway falls back to the
  existing `query + documents` heuristic and flags it
  `"tokens_estimated": true`.
- `POST /v1/rerank` accounting (`lumen_tokens_total{...,estimated}` and
  `usage_log.estimated`) now reflects whether the *token* count was
  upstream-reported or gateway-derived, rather than always `true`.

### Added - Cloudflare Workers AI rerank (native endpoint)

- The `cloudflare` kind now serves **rerank** in addition to chat/embed.
  Workers AI's `bge-reranker-*` models are not part of the OpenAI-compatible
  surface, so reranking is translated against Cloudflare's native
  `POST /ai/run/{model}` endpoint (`{ query, contexts, top_k }` in,
  `{ result: { response: [{ id, score }] }, success, errors }` out) rather
  than the OpenAI-compatible path used for chat/embed. One `[[providers]]`
  entry with `kind = "cloudflare"` now serves all three capabilities against
  the same account-scoped `base_url`; the native endpoint's URL is derived by
  stripping a trailing `/ai/v1` (or `/v1`) suffix to reach the account root.
  Cloudflare reports no token usage for this model, so `usage` follows the
  same ADR 003 fallback as TEI (a gateway-derived estimate, marked
  `estimated`).

### Fixed

- **Dedicated client-cancel error code (issue #11).** A client-initiated
  cancel (`ProviderError::Cancelled`, typically a disconnect mid-request) no
  longer maps to `GatewayError::Internal` (`LM-5001`, 500). It now has its own
  `GatewayError::ClientCancelled` (`LM-6001`, HTTP 499,
  `type: client_cancelled`), documented in `docs/errors.md` and
  `docs/adr/006-client-cancellation-error-code.md`. `499` (the conventional
  "client closed request" status) keeps it out of the `5xx` class entirely, so
  `lumen_http_request_duration_seconds`/`lumen_request_duration_seconds` and any
  alert built on `status=~"5.."` no longer count a client hanging up as an
  internal gateway malfunction.
- A mid-stream client disconnect now settles the request's accounting record
  (`usage_log.status` and the `lumen_request_duration_seconds` sample) at 499
  instead of a hardcoded 200: previously the most common real-world cancel
  was silently recorded as a success. A stream whose `data: [DONE]`
  terminator was already delivered still settles as 200 even if the client
  disconnects immediately after.
- **LM-2004 pre-flight now covers the whole fallback chain, not just the
  primary route.** The remote-image-URL check in the chat handler only
  inspected `chain[0]`; if the primary provider accepted remote URLs (e.g.
  OpenAI) but a fail-over reached an image-incapable model (e.g. Gemini), the
  fallback's translation failure surfaced as a generic `LM-3002` (502) instead
  of the honest `LM-2004` (400) client error. Added a dedicated
  `ProviderError::ImageUrlNotSupported` variant (deterministic, never
  retried, never faults the circuit breaker - matching `Translation`'s
  fallback-stopping semantics) so this specific failure is classified
  correctly no matter which link in the chain hits it, without eagerly
  rejecting requests a fallback would never actually need to serve (GH #13).
- Anthropic chat responses (both `POST /v1/chat/completions` and its SSE
  stream) now stamp `created` with a real unix timestamp instead of a
  hardcoded `0`. New shared `providers::mapping::unix_timestamp` helper
  (clamped to `0` on a pre-epoch clock, no panics) backs both the
  non-streaming and streaming translation paths.

### Added - Endpoint latency observability

- **Every endpoint now measures and publishes its latency.** A new middleware
  times each HTTP request (including `/health`, `/health/providers`,
  `/metrics`, `/v1/models` and `/admin/*`) and exports
  `lumen_http_request_duration_seconds{method,path,status}` - `path` is the
  matched route template (or `"unmatched"`), never the raw URI, so cardinality
  stays bounded and user data never reaches a label. Each request also emits a
  `lumen::http` "request completed" log event carrying `latency_ms` inside the
  request span. For streaming responses this layer measures
  time-to-response-headers.
- New `lumen_request_duration_seconds{capability,model,provider,status}`
  histogram: end-to-end latency of accounted API calls (chat/embed/rerank),
  attributed to the model/provider that actually served the request (fallbacks
  included). Recorded when accounting closes, so streaming chat covers the
  full stream - the Prometheus counterpart of `usage_log.latency_ms`.
- The `lumen::usage` structured log event now includes `latency_ms`, from the
  same clock read as the usage-log row and the histogram sample.

### Added - Multimodal embeddings + guarded image fetch (M9)

- `POST /v1/embeddings` now accepts image inputs via OpenAI-style content parts:
  `input` may be an array whose items are strings or arrays of typed parts
  (`{"type":"text",...}` / `{"type":"image_url",...}`), mixable per item. The
  part `type` defaults to `"text"`, and text-vs-image is decided by which field
  is present, not by `type`. Text-only `input` (string or string array) is
  unchanged. Reuses the shared `ContentPart`/`ImageUrl` types from
  `crates/core/src/chat.rs` (introduced by M8 chat vision).
- Per-model `modalities` config (default `["text"]`), surfaced in
  `GET /v1/models`. Image input to a model without `"image"` fails fast with
  `LM-2003` (400) before any upstream call.
- Multimodal translation for Cohere (embed-v4 `inputs`/`content`), Voyage
  (`/multimodalembeddings`), and Jina (object `input` array). Non-image-capable
  providers are gated by the `modalities` check.
- **Opt-in, guarded server-side image fetch** (`[image_fetch]`, default off):
  a remote `http(s)` image URL is fetched, base64-encoded, and inlined as a
  `data:` URI before provider translation. Guards: scheme/host/prefix
  allowlists, a non-configurable private/loopback/link-local IP block with the
  connection pinned to the vetted resolved address (DNS-rebinding safe), a
  streamed size cap, a per-fetch timeout, an `image/*` MIME allowlist, and
  redirect re-validation. Cancellation-aware. New error codes `LM-2005`
  (fetch disabled), `LM-2006` (rejected by a guard), `LM-2007` (fetch failed).
  A remote URL never leaks internal network detail in the client error.
- Token accounting (ADR 003) for multimodal: upstream `usage` is trusted; the
  local fallback estimates text parts only (images contribute 0, flagged
  `estimated`).
- **Media accounting** as a billing dimension alongside tokens: each request's
  media item count and total **decoded** bytes are measured (per top-level type)
  and exported as Prometheus counters `lumen_media_total` and
  `lumen_media_bytes_total` (labels `capability`/`model`/`provider`/`media_type`
  + the metadata allowlist), added to the `lumen::usage` structured log, and
  persisted to new `usage_log` columns `media_count` / `media_bytes` (migration
  `0003`). Measured uniformly whether the image was a client `data:` URI or
  gateway-fetched.

### Added - Vision (image input to chat)

- `POST /v1/chat/completions` accepts OpenAI's content-parts message shape
  (`content` as a string *or* an array of `{"type":"text"|"image_url",...}`
  parts); unknown future part types (e.g. `input_audio`) survive round-trip
  verbatim rather than erroring. `MessageContent`/`ContentPart`/`ImageUrl` land
  in `crates/core/src/chat.rs`.
- Per-model opt-in: `modalities = ["text", "image"]` in `[[providers.models]]`
  (default `["text"]`), surfaced in `GET /v1/models`. An image part sent to a
  model that hasn't opted in is rejected with the new `LM-2003` (400) before
  any upstream call.
- **Provider translation:** OpenAI-family kinds (+ `vllm`) forward image parts
  verbatim; **Anthropic** translates `image_url` to `image` source blocks
  (base64 or `url`, both directions); **Gemini** translates to `inline_data`
  (base64 only) - a remote image URL routed to Gemini is rejected with the new
  `LM-2004` (400) rather than the gateway fetching it itself. LUMEN never
  dereferences a user-supplied image URL (SSRF-safety + the latency pillar);
  only providers that fetch remote URLs themselves (OpenAI, Anthropic) may
  receive one.
- **Accounting** (ADR 003 addendum): upstream-reported `usage` already folds in
  image tokens and is trusted as-is; the local estimation fallback counts text
  only (`MessageContent::text()`), so an image part contributes `0` to an
  estimate - the response is still honestly flagged `"estimated": true`, never
  a silent zero. A per-image token heuristic is deferred (`docs/backlog.md`).
- `LM-1002` request-body-size envelope (previously a raw 413) now wraps every
  route, including chat - base64-inlined images can be large.
- Docs: `docs/errors.md` (`LM-2003`/`LM-2004`), a new "Vision (image input)"
  section in `docs/providers.md`, README capability note, and a commented
  `modalities` example in `config.example.toml`.

### Added - OpenAI-compatible provider kinds

- Eleven new `kind`s served by the OpenAI provider with a per-kind base URL:
  `groq`, `together`, `fireworks`, `deepseek`, `openrouter`, `perplexity`,
  `xai`, `deepinfra`, `huggingface` (the HF Inference router), `cloudflare`
  (Workers AI - `base_url` carries the account id, so it is required), and
  `vllm` (any self-hosted OpenAI-compatible server; `base_url` required, API key
  optional). All serve chat + embeddings. `ProviderKind` gains
  `default_base_url()` and `is_openai_compatible()`; a missing URL for the two
  URL-required kinds is a boot error rather than a silent fall-through to
  api.openai.com. Docs (`docs/providers.md`, README matrix) and registry +
  server wiring tests included.

## [0.1.0] - 2026-07-13

First tagged release. LUMEN is a universal, self-hostable LLM gateway in
Rust - chat, embeddings and reranking as first-class capabilities behind one
OpenAI/Cohere-compatible surface, with a measured **~3 µs** added CPU per
request off-network, **~8.8 MB** idle RAM, hard budgets, end-to-end
cancellation, and zero telemetry. See the entries below for the full feature
history.

### Added - Release (hot reload, packaging, security, benchmarks)

- **Config hot reload** (§7.3): `SIGHUP` or a config-file change re-validates and
  atomically swaps the provider routing table via the registry's ArcSwap;
  in-flight requests are untouched. An invalid reload keeps the running config
  and increments `lumen_config_reload_failures_total`. Scope: the routing
  table (server/auth/pricing/resilience stay boot-time).
- **Packaging** (§7.2): multi-stage `Dockerfile` → static musl binary on
  `distroless/static:nonroot` (no shell, no libc). `release.yml` builds musl
  binaries (x86_64 + aarch64, capped at 25 MB) on `v*` tags and buildx-pushes a
  multi-arch image to GHCR.
- **Default security headers** (§7.4): `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
  `Content-Security-Policy: default-src 'none'` on every response; `SECURITY.md`
  documents the disclosure policy and security model.
- **Supply chain** (§7.4): `cargo audit` + `cargo deny` (permissive-license
  allowlist, advisory/source pinning) in CI; a standalone `fuzz/` crate with
  libfuzzer targets for the SSE and request parsers, run 10 min/target weekly.
- **Benchmarks** (§7.1): a Criterion `gateway_overhead` bench and
  `docs/perf-baseline.md` recording the measured in-process overhead, idle RAM
  and binary size; `bench/` is a reproducible docker-compose + k6 head-to-head
  vs LiteLLM.
- **New metrics**: `lumen_config_reloads_total`,
  `lumen_config_reload_failures_total`.

### Added - Resilience (retries, fallback, circuit breaker, timeouts, health)

The gateway now survives flaky upstreams without becoming
flaky itself: retries, multi-provider fallback, a per-provider circuit breaker,
per-phase timeouts and optional background health checks - none of it on a
database path, and `/health` stays independent of provider health (the LiteLLM
#15526 lesson: a 429 storm must not destabilise the gateway).

- **Retries** (`[resilience]`): retryable upstream failures only (5xx,
  connect/read timeout, 429 - *never* a client 4xx) with exponential backoff +
  equal jitter (`retry_base_ms` 200, `retry_max_ms` 5000, `retry_max_attempts`
  3), honouring an upstream `Retry-After` as a floor. The backoff maths are a
  pure, unit-tested function fed a lock-free splitmix64 fraction - no
  dependency, no blocking, no clock read on the hot path.
- **Fallback chains** (§6.2): per-model `fallbacks = ["model-b", …]`, tried in
  order once the primary's retries are spent or its circuit is open. Each
  fallback must exist and serve every capability of the model it backs
  (validated at boot). Responses carry `x-lumen-model-used` and
  `usage_log.model_used` records which model actually served; metrics and cost
  attribute to the served model.
- **Circuit breaker** (§6.3): per (provider, model), Closed → Open (after
  `circuit_failure_threshold` 5 consecutive faults) → Half-Open (after
  `circuit_cooldown_ms` 30 000, exactly one probe) → Closed/Open. In-memory,
  the lock never held across an await; state exported as
  `lumen_circuit_state{provider,model}` (0/1/2). Open with no fallback left
  → 503 `LM-3020` with `Retry-After`.
- **Timeouts** (§6.4): `connect_timeout_ms` (5000, client-wide → `LM-3012`),
  `first_token` (the `server.first_token_timeout_ms`, 30 000 → `LM-3011`) and
  `total_timeout_ms` (600 000 → `LM-3013`), each a distinct code. `first_token`
  and `total` are overridable per provider; `connect` is client-wide (one
  pooled HTTP client). All bounded by the executor's absolute total deadline.
- **Streaming stays committed once it starts**: retry/fallback happen only while
  *opening* the upstream byte stream; after the first frame the stream guards
  (LM-3010/3011, heartbeat) own the stream and never retry.
- **Background health checks** (§6.5, `health_check_enabled` off by default):
  a periodic probe of every provider with a configured `base_url` fills
  `GET /health/providers` and `lumen_provider_up{provider}`; vendor-default
  URLs report `unknown` (never probed). Entirely off the request path.
- **Errors**: `LM-3012` (connect timeout, 504), `LM-3013` (total timeout, 504),
  `LM-3020` (circuit open, 503). `ProviderError` gained `is_retryable` /
  `is_provider_fault` classification shared by the retry loop and the breaker.
- **ADR 005** records the execution model (one generic chain executor,
  capability-specific resolution, breaker placement, streaming boundary, the
  connect-per-provider and first-frame-peek simplifications deferred).

### Added - Auth, virtual keys, hard budgets & token accounting

The gateway can now be shared safely: keys, budgets that
can NEVER be overrun, and a token count for every single request.

- **Virtual keys** (`[auth]`, off by default): `fg-` + 32 random bytes,
  stored as a BLAKE3 hash only (the keys are 256-bit random - a password KDF
  would just burn hot-path CPU). Auth on all of `/v1/*`; unknown, disabled
  and expired keys are one indistinguishable `LM-4004` (401). `/health` and
  `/metrics` stay open.
- **Hard budgets, enforced in memory** (§5.2): the pre-call cost estimate
  is *reserved* with an atomic CAS before the upstream call and settled to
  the real usage after it - 50 concurrent requests against a budget for 10
  admit exactly 10 (tested at both the atomic and the HTTP level). Refusals
  happen BEFORE any upstream traffic: 402 `LM-4001` (budget), 429 `LM-4002`
  (RPM) / `LM-4003` (TPM, with `Retry-After`). The DB is never consulted on
  the request path; budgets flush to SQLite periodically (default 10 s - a
  crash loses at most that much *accounting*, never allows an overrun) and
  reload at boot, so an exhausted key stays exhausted across restarts.
- **Token accounting always on** (ADR 003): every chat/embed/rerank call
  yields a count - upstream usage when reported (`estimated=false`), else a
  byte-heuristic estimate flagged `estimated: true` in the response body, in
  `lumen_tokens_total{capability,model,provider,direction,estimated}`
  and in `usage_log`. TEI's report-nothing embeddings now count > 0.
  Streaming chat sniffs the final usage chunk with bounded state (no
  response accumulation); rerank counts search units (upstream-billed or
  derived, never silently zero). The opt-in accurate tokenizer stays in the
  backlog - only the O(bytes) heuristic runs, inline and hot-path-safe.
- **Cost counting** (§5.4b): per-model prices in config (`cost_per_1m_input`,
  `cost_per_1m_output`, `cost_per_1k_searches`) feed both the budget
  reservation and the `usage_log.cost` column. Unpriced models cost 0.
- **Async usage log** (§5.3): bounded mpsc (default 10 000) → batched writer
  (500 entries / 2 s); a full channel drops the entry and bumps
  `lumen_usage_log_dropped_total` - the request path NEVER blocks on
  logging. Background retention purge (default 30 days). No prompt/response
  content is ever stored.
- **Request metadata** (ADR 002): `x-lumen-metadata` (alias
  `cf-aig-metadata`), a flat JSON object bounded at 16 keys / 64 B keys /
  256 B values / 4 KiB, parsed once at the edge. Full object → structured
  logs + `usage_log.metadata`; ONLY `telemetry.metadata_labels` allowlist
  keys become Prometheus labels (default empty - client metadata can never
  mint a time series). Malformed metadata never fails the request: dropped
  with a warn + `lumen_metadata_rejected_total`.
- **Admin API** (§5.5), mounted only when auth is on and gated by
  `LUMEN_MASTER_KEY` (64 hex chars, compared by hash): `POST/GET
  /admin/keys`, `PATCH /admin/keys/{id}` (changes apply immediately, no
  restart), and `PUT /admin/provider-keys/{name}` to store provider keys
  AES-256-GCM-encrypted at rest (env vars remain the default; DB keys
  back-fill env-less providers at boot). Key creation is the single moment
  the plaintext exists.

### Changed

- **LM-4xxx codes realigned to the spec** (they were placeholders, never
  emitted): `LM-4001` = budget exhausted (402), `LM-4002` = RPM (429),
  `LM-4003` = TPM (429), `LM-4004` = missing/invalid key (401).
- `Usage`, `EmbedUsage` and `RerankUsage` gained an optional `estimated`
  field, omitted unless the gateway estimated the counts (ADR 003).

### Added - Streaming translation, tools, and stream guards

This closes every remaining streaming criterion:

- **Incremental SSE parser** (`providers::sse`): reassembles upstream events
  fragmented across TCP packets (LF and CRLF, multi-line `data:`, comments
  ignored), buffering only the current incomplete event with a hard size cap.
- **Anthropic streaming translation**: typed events (`message_start`,
  `content_block_start/delta`, `message_delta`, `message_stop`) → OpenAI
  chunks, including streamed **tool_use** (`input_json_delta` → `tool_calls`
  argument deltas, OpenAI indices allocated in order of appearance). Bounded
  state - the response text is never accumulated. In-stream `error` events
  propagate only the upstream error *type*, never message bodies.
- **Anthropic tools, both directions** (criterion 3): OpenAI `tools` →
  Anthropic `tools` (+ `tool_choice` mapping), assistant `tool_calls` →
  `tool_use` blocks, role `tool` → `tool_result` blocks (consecutive results
  merged into one user message); response `tool_use` blocks → OpenAI
  `tool_calls` with `arguments` re-encoded as a JSON string. Verified by an
  exact-JSON snapshot test.
- **Gemini streaming** (`streamGenerateContent?alt=sse`): partial responses →
  OpenAI chunks; the final fragment carries `finish_reason` + full usage.
- **Stream guards** in the server (all configurable):
  - *first-token timeout* (`first_token_timeout_ms`, default 30 s) → LM-3011:
    a plain 504 when the upstream never answered, an SSE error frame when the
    stream had started; non-streaming applies the window to the whole upstream
    call (per-phase timeouts come later);
  - *missing terminator* → LM-3010 error frame when the upstream dies without
    `data: [DONE]` (criterion 5) - detection survives a `[DONE]` split across
    frame boundaries; the gateway never fabricates the terminator itself;
  - *heartbeat* (`sse_heartbeat_ms`, default 15 s): `: ping` comments on idle
    streams so proxies don't reap slow upstreams.
- **Streaming usage (ADR 003), upstream half**: passthrough requests
  `stream_options.include_usage`; translated providers emit full usage in the
  final chunk. The local-estimation fallback (`estimated=true`) lands later
  with the Prometheus counters and `usage_log`.

### Added - Google Gemini + Mistral embeddings

- **Google Gemini** chat provider (non-streaming) with bidirectional
  translation: OpenAI messages → `contents` (assistant→`model`, system hoisted
  to `systemInstruction`), params → `generationConfig`, response `candidates`/
  `finishReason`/`usageMetadata` → OpenAI shape. Auth via `x-goog-api-key`
  header; the model rides in the URL path, the key never does.
- **Mistral embeddings** (`EmbeddingProvider`, OpenAI-compatible passthrough) -
  Mistral now serves both chat and embeddings; added to the embeddings
  conformance suite.

  *Still remaining:* Anthropic + Gemini streaming-event
  translation (criterion 4), first-token timeout LM-3011 (criterion 6),
  upstream-closes-without-`[DONE]` → LM-3010 (criterion 5), SSE heartbeat, and
  streaming token estimation (ADR 003).

### Added - Zero-copy SSE streaming

- Real incremental streaming for `stream=true`: the gateway forwards the
  upstream SSE bytes **verbatim** - no per-chunk `serde` round trip (ADR 004).
  New `ChatProvider::chat_stream_bytes` (default serializes the typed
  `chat_stream`; OpenAI/Mistral override it to pipe `reqwest`'s `bytes_stream`
  via the shared `http::open_stream`). The server writes a raw `Bytes` body
  (`Body::from_stream`, `content-type: text/event-stream`) with the cancel
  drop-guard moved inside it, so a client disconnect aborts the upstream.
  Proven byte-identical over 100 chunks; `stream_options.include_usage` is
  requested automatically without overriding a client's choice.

  *Still deferred:* Anthropic streaming-event translation, Google
  Gemini, Mistral embeddings, first-token timeout (LM-3011), SSE heartbeat, and
  streaming token sniffing/estimation (ADR 003).

### Added - Chat completions (non-streaming)

- `POST /v1/chat/completions`: non-streaming JSON end to end (validate → route →
  provider → OpenAI-shaped response), and a functional streaming SSE path
  (`text/event-stream`, `data: {...}` frames, terminal `data: [DONE]`, 15 s
  keep-alive pings). Client disconnect cancels the per-request token and aborts
  the upstream (the drop guard is moved into the SSE body stream).
- Chat providers: **OpenAI** and **Mistral** (OpenAI-compatible passthrough),
  and **Anthropic** with non-streaming bidirectional translation (system hoisted
  to the top-level field, `max_tokens` defaulted, `stop`→`stop_sequences`,
  `stop_reason`→`finish_reason`, `input/output_tokens`→`usage`; auth via
  `x-api-key`/`anthropic-version`, not bearer).
- Chat routing (`resolve_chat`, LM-2001/LM-2002) and registry chat routes; a
  shared `chat::single_shot_stream` adapter backs the interim `chat_stream`.
- Reserved streaming error codes `LM-3010` (upstream stream interrupted, 502)
  and `LM-3011` (first-token timeout, 504) in the taxonomy and `docs/errors.md`.

  *Deferred to the streaming work:* zero-copy incremental SSE passthrough,
  Anthropic streaming-event translation, Google Gemini, Mistral embeddings, the
  first-token timeout, and streaming token estimation (ADR 003).

### Changed

- `http::post_json` gained a header-based sibling `post_json_with_headers` (for
  Anthropic's non-bearer auth); the two share one send/classify core.

### Added - Reranking & model discovery

- `POST /v1/rerank` (Cohere wire format): `documents` accept bare strings or
  `{ "text": ... }` objects; the gateway guarantees the client-facing invariants
  regardless of upstream behaviour - results sorted by descending
  `relevance_score`, `top_n` clamped to the document count then truncated,
  `document` echoed only when `return_documents` is set (off by default). Empty
  `documents` is rejected with `LM-2010` (400) before any upstream call.
- Four new providers, each implementing **both** `EmbeddingProvider` and
  `RerankProvider`: **Cohere** (v2 `embed`/`rerank`), **Jina**
  (OpenAI-compatible embed, Cohere-shaped rerank), **TEI** (self-hosted, keyless,
  bare-array `/embed` and `/rerank`), and **Voyage** (`top_k`/`data[]` rerank).
- A generic **rerank conformance suite** all four providers pass identically
  (ordering, 429/`Retry-After`, 5xx, malformed response, cancellation) - the
  rerank counterpart of the embeddings harness.
- `GET /v1/models`: OpenAI-shaped list extended with a `capabilities` array,
  reflecting only the operator's configuration (no upstream introspection); a
  single Cohere model configured for embed+rerank appears with both.
- Versioned aliasing hardened: a duplicate model id now aborts startup with a
  message naming **both** conflicting providers; several aliases may map to one
  `upstream_id`. `config.example.toml` demonstrates every rerank provider.

### Tooling

- Adopted a session-start dependency-freshness step (`rustup update` +
  `cargo outdated`); documented in the work loop. Toolchain moved to Rust
  **1.97.0** (from 1.95.0) - clippy pedantic and the full suite stay green.
- Planned a Cloudflare-style per-request metadata header
  (`x-lumen-metadata`) for logs, `usage_log` and cardinality-bounded
  Prometheus labels - design in ADR 002, tasks folded into a later change.
- Elevated **token accounting** to a first-class, always-on promise: every
  request of every capability yields a token count (upstream usage when present,
  else a labelled local estimate - never a silent zero, e.g. TEI), surfaced in
  the response, Prometheus counters and `usage_log`. Design in ADR 003; tasks
  threaded through the streaming work (extraction) and the auth/budgets work
  (counters, estimation, storage), and added to the mission pillars and ROADMAP.

### Changed

- Extracted a shared `http::post_json` helper (transport + error classification)
  that every provider now shares, including OpenAI and Ollama (behaviour
  unchanged); only body translation differs per provider.
- Added `LM-2010` (empty rerank `documents`, 400) to the taxonomy and
  `docs/errors.md`, and a `Voyage` variant to `ProviderKind`.

### Added - Embeddings (first complete request path)

- `POST /v1/embeddings` end to end (OpenAI wire format): validate → route →
  provider → response, with the client model id resolved to its upstream alias.
- OpenAI embeddings provider (the canonical reference) and a keyless Ollama
  provider, both driven by a shared, pooled rustls HTTP client.
- A generic embeddings **conformance suite** that both providers pass
  identically (nominal, batching-in-order, 429/`Retry-After`, 5xx, malformed
  response, cancellation) - the reusable harness every future provider must pass.
- Provider **registry** behind `ArcSwap` (ready for hot reload) that builds
  instances from config-derived specs and resolves `(capability, model)`; the
  **router** turns misses into `LM-2001` (unknown model, 404) or `LM-2002`
  (capability mismatch, 400).
- Automatic **batching**: requests over a provider's `max_batch_size` split into
  sub-batches run with bounded concurrency (default 4), reassembled in original
  order with summed usage; any sub-batch failure fails the whole request.
- End-to-end **cancellation**: a per-request `CancellationToken` is cancelled on
  client disconnect and aborts the in-flight upstream call.

### Changed

- Error taxonomy realigned to the codes pinned by the spec: `1xxx` request,
  `2xxx` routing (`LM-2001`/`LM-2002`), `3xxx` upstream (`LM-3001` rate-limited,
  `LM-3002` malformed-response → 502, plus generic/unavailable/timeout), `4xxx`
  auth/budget, `5xxx` internal. Added a `ProviderError::Unavailable` variant for
  transport failures (→ 503). `docs/errors.md` updated.
- `ProviderKind` moved from the server config into the `providers` crate (it is
  the registry's construction discriminant); crate package names stay bare.

### Added - Skeleton & foundations

- Cargo workspace with six crates (`core`, `providers`, `router`, `auth`,
  `telemetry`, `server`), release profile tuned for a small, fast binary
  (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip`), pinned
  stable toolchain, and Apache-2.0 license.
- `core`: capability traits (`ChatProvider`, `EmbeddingProvider`,
  `RerankProvider`) taking a `CancellationToken`; OpenAI-shaped chat/embeddings
  types and Cohere-shaped rerank types (unknown fields preserved for
  passthrough); the `Capability` enum; and the two-layer error taxonomy
  (`ProviderError` → `GatewayError`) with stable `LM-XXXX` codes and a standard
  JSON error envelope.
- `telemetry`: a Prometheus registry wrapper and structured-logging setup.
- `server`: axum binary with `GET /health` (no I/O, always 200 while alive) and
  `GET /metrics`; per-request `x-request-id`, tracing spans (metadata only -
  never body or query string), and a configurable body-size limit; bounded
  graceful shutdown on SIGINT/SIGTERM (30 s drain).
- Configuration via figment (TOML + `LUMEN_*` env overrides) with
  boot-time validation that exits non-zero naming the offending field; API keys
  are referenced by env-var name only, never stored. Commented
  `config.example.toml`.
- GitHub Actions CI: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -D warnings` (pedantic), and `cargo test --workspace`.
- Docs: error-code reference (`docs/errors.md`), ADR 001 (crate/lib naming),
  and this changelog.

[0.1.0]: https://github.com/qdequele/lumen/releases/tag/v0.1.0
