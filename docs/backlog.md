# Backlog

Ideas surfaced during development that are intentionally out of scope for v1
(see `CLAUDE.md` → "What we do NOT do (v1)" and `ROADMAP.md` → "Backlog v2").
Recorded here so they are not lost, and so we don't gold-plate the current
milestone.

## Noted while building ADR 009 (shared parent budgets)

- **Groups carry budget only.** Group-level RPM/TPM, a group `disabled`
  flag (pause a whole customer), group expiry, and nested groups are all
  deliberate non-goals of the first slice; each is a natural follow-up on
  the same `GroupEntry` shape.
- **No `lumen groups` offline subcommand.** Groups are created via
  `POST /admin/groups` (master-key gated, no virtual key needed), so there
  is no bootstrap chicken-and-egg; `lumen keys create --group-id` covers
  the offline key path. Add the subcommand only if a real workflow needs
  fully offline group provisioning.
- **No `lumen_group_budget_remaining` gauge.** Pool spend is visible via
  `GET /admin/keys`-style reads (`GET /admin/groups`) and the usage log;
  a Prometheus gauge per group would be cheap but adds a per-group metric
  series - decide when a dashboard actually wants it.
- **Atomic budget top-up.** `PATCH` on a key or group budget is a
  read-modify-write for the caller; a `POST .../grant {"amount"}` increment
  route would remove the need to serialize concurrent top-ups in the
  control plane.

## Deferred to v2 (from the vision)

- Web admin UI
- Semantic cache
- Audio (input/output) support. Image input already shipped in v1:
  [chat vision](chat/vision.md) (M8) and
  [multimodal embeddings](embeddings/multimodal.md) (M9).
- Guardrails / moderation
- Distributed rate limiting (Redis)
- OTLP tracing export
- WASM plugin system
- Postgres backend for the auth/usage store (a `postgres` sqlx feature flag;
  the queries in `crates/auth` are simple enough to stay portable, so this is
  cheap to add if a deployment ever needs it - v1 is SQLite only)
- `/v1/batches` (OpenAI async batch jobs API) - explicit v1 non-goal: batch
  jobs imply persistent job state and scheduling, which sits poorly with the
  DB-off-the-request-path and single-binary pillars. Note:
  `crates/providers/src/batch.rs` is embedding request sub-batching, unrelated
  to this API surface.
- `/v1/files` (OpenAI file upload/storage API) - explicit v1 non-goal: blob
  storage state, same rationale as `/v1/batches`.

## Noted while building M1

- ~~Token-array inputs for `/v1/embeddings` (`input` as arrays of token ids) are
  not modelled - only string and string-batch.~~ **Resolved (issue #25).**
  `EmbedInput` now models `Tokens` (`[1,2,3]`) and `TokenBatch` (`[[1,2],[3,4]]`);
  they pass through natively on OpenAI-compatible providers and count one token
  per id in the estimation fallback. Text-only providers (Cohere, TEI, Ollama,
  Jina, Voyage, Mistral) reject them with a 400 (LM-1001) before any upstream
  call.
- ~~Rerank `documents` accepts only strings; Cohere also allows objects.~~
  **Resolved (issue #25).** See the M3 note below.
- `error_type()` collapses 401/402/429 into `invalid_request` because the public
  taxonomy only has three `type`s. Fine per `CLAUDE.md`, but note it's coarse.
- Acceptance criterion "boot < 100 ms" is verified manually (M1); fold a real
  timing assertion into the M7 criterion benchmarks rather than a flaky unit test.
- ~~Graceful shutdown is unit-tested via an injected shutdown future; the real
  SIGINT/SIGTERM path (`shutdown_signal`) has no integration test (hard to do
  portably). Acceptable; revisit if signal handling grows.~~ **Resolved**
  (issue #27). `crates/server/tests/signal_shutdown.rs` (`#[cfg(unix)]`) spawns
  the real `lumen` binary and sends it a genuine `SIGTERM`/`SIGINT` via
  `libc::kill`, asserting the same drain-then-exit-0 behaviour the injected-
  oneshot tests already prove for `serve()` itself - now proven for the actual
  `tokio::signal` path too.

## Noted while building M2

- ~~Embedding output is always a float array in v1. Base64 embeddings are decoded
  on the way IN (a client requesting `encoding_format: "base64"` won't error),
  but we do not re-encode on the way OUT.~~ **Resolved (issue #25).** When
  `encoding_format: "base64"` is requested, the gateway re-encodes each vector as
  OpenAI-style base64 at the response edge, so it works for every provider
  (including Ollama and TEI, which have no upstream `encoding_format`).
- ~~Ollama drops the OpenAI-only `dimensions` field with a `debug!` log; a client
  asking for a specific dimension silently gets full-width vectors.~~ **Resolved
  (issue #25).** A per-provider `strict = true` makes Ollama reject a request that
  sets `dimensions` with a 400 (LM-1001) instead of silently dropping it; the
  default stays lenient. `encoding_format` is no longer lost either (handled at
  the edge, above).
- `LM-1002` (payload too large, 413) is emitted by `RequestBodyLimitLayer` as a
  raw 413 without our JSON error envelope. Map the tower-http rejection to
  `GatewayError::PayloadTooLarge` for a consistent body.
- Cancellation tests use real (short) wall-clock delays rather than
  `tokio(start_paused)`; robust today but revisit if they flake under CI load.
  The HTTP-level disconnect test asserts the server stays responsive and the
  upstream got the request - the actual upstream abort is proven at the provider
  layer (conformance `scenario_cancellation_aborts_upstream`).
  **Update (issue #27):** this predicted flake happened -
  `mistral_passes_embed_conformance_suite` flaked once under full
  workspace-test parallelism. Widened the mocked upstream delay (2s → 3s) and
  the elapsed-time assertion (1s → 2s) in `scenario_cancellation_aborts_upstream`
  (`crates/providers/tests/embeddings.rs`) for more scheduler-jitter headroom
  without weakening what the assertion proves (still asserts the call returns
  in well under half the mocked delay). `tokio(start_paused)` would sidestep
  wall-clock entirely but doesn't compose with the real `reqwest`/wiremock I/O
  this suite exercises; deferred unless the wider margin still flakes.

## Noted while building M3

- Cohere v2 embed requires an `input_type`; the gateway can't know query-vs-
  document intent by default, so it sends `search_document` unless overridden.
  Resolved (issue #22): a caller may set `input_type` as an extra field on the
  `/v1/embeddings` request body (`search_document`, `search_query`,
  `classification`, or `clustering`); an unknown value is rejected with
  `LM-1001` before any upstream call. See `docs/providers.md` § cohere. A
  per-model default (config-side) is still open if per-request opt-in proves
  insufficient in practice.
- `usage.search_units` is only meaningful for Cohere; Jina and Voyage bill
  rerank in tokens. Resolved (issue #10): `RerankUsage` now carries a separate
  `total_tokens`/`tokens_estimated` pair, upstream-reported for Jina/Voyage and
  gateway-derived (from `query + documents`) for every other provider.
- ~~Rerank `documents` accept string or `{text}` only. Cohere also allows
  arbitrary objects with a `rank_fields` selector - out of scope.~~ **Resolved
  (issue #25).** `RerankDocument::Object` now keeps all fields; the request
  carries an optional `rank_fields` selector, and the gateway reduces each object
  document to a single ranking text at the edge (selected fields joined, or the
  `text` field when no selector), so providers still only ever see plain text.
  With `return_documents: true`, an object document echoes that reduced ranking
  text in `document.text`, not the original JSON object.
- TEI serves one model per process and ignores the request `model`/`top_n`; the
  gateway truncates to `top_n` after sorting. The configured `upstream_id` is
  informational for TEI. A future health/introspection hook could verify the
  configured model matches what the TEI process actually serves.
- The four hosted rerank providers default `max_batch_size` conservatively
  (Cohere 96, Jina/Voyage/OpenAI-style large, TEI 32). Revisit against real
  provider limits; embeddings batching already exercises these.
- **Per-model embedding `max_batch_size` override (surfaced by issue #90).**
  Vertex embeddings hardcode `max_batch_size() = 1` because
  `gemini-embedding-001` accepts a single instance per `:predict` call, but the
  other `text-embedding-*` models take up to 250. A single conservative value
  means a 1000-input request is ~250 sequential rounds at concurrency 4. A
  config override (or a larger known-safe default for the text-embedding
  models, keyed by model) would soften this bottleneck. Deferred: it needs
  per-model config plumbing that the current `EmbeddingProvider::max_batch_size`
  (provider-wide, model-agnostic) does not carry.

## Noted while building M4 (slice 1 - non-streaming chat)

- **Streaming disconnect test is a no-hang assertion, not an abort assertion.**
  `streaming_client_disconnect_does_not_hang_server` proves the server stays
  responsive but not that the upstream connection was actually closed (M4
  acceptance criterion 2: "upstream closed in < 100 ms"). And because the interim
  single-shot `chat_stream` awaits the full `chat()` before the guard is moved
  into the SSE body, the moved-guard path is not exercised. Strengthen in the
  streaming slice: assert via wiremock that the upstream request was aborted,
  and add a case where the client cuts *after* the body starts.
- Anthropic `translate_request` copies message roles verbatim and does not
  normalise user/assistant alternation or drop/merge `tool`/empty-content
  messages (spec 4.3 bullet). Fine for the interim text path; complete with
  tool translation in the streaming slice.
- Anthropic responses set `created: 0` (the API returns no timestamp). Some
  OpenAI clients expect a real epoch; set `SystemTime::now()` if one complains.
- `to_sse_body`'s `Chain` drops the mapping closure (and thus the cancel guard)
  as soon as the chunk stream is exhausted, a hair before the `[DONE]` frame.
  Benign today (nothing left to cancel once the upstream is done), but the real
  streaming slice must not tie a live resource to guard survival through `[DONE]`.
- Interim single-shot emits `[DONE]` even after a mid-stream error frame.
  Harmless with one item; real streaming must terminate after an error.

## Noted while building M4 (slice 2 - zero-copy streaming)

- ~~**Acceptance criterion 5 (LM-3010) not yet implemented.**~~ **Resolved**
  (commit `076b909`, slice 3). When the upstream closes without a `[DONE]`
  terminator and without a transport error, the gateway now appends a terminal
  `data: {"error": {"code": "LM-3010"...}}` frame then closes cleanly. The
  lightweight tail-watcher (`EventStreamState::scan_frame` in
  `crates/server/src/chat.rs`) inspects only frame boundaries - it matches a
  line-anchored `\ndata: [DONE]` marker and keeps at most `DONE_MARKER.len() - 1`
  trailing bytes, so a terminator split across two frames is still detected and
  model content that merely contains the text can never spoof it. Covered by the
  unit guard tests in that module and the end-to-end wiremock test
  `upstream_stream_without_done_yields_fg3010_error_frame` (passthrough), plus
  the mid-stream error-frame tests (`mid_stream_provider_error_becomes_terminal_error_frame`
  and `resilience.rs`).

- **Tools on Gemini**: RESOLVED (issue #4). `translate_request` (google) now
  maps OpenAI `tools` to Gemini `tools[].functionDeclarations` and `tool_choice`
  to `toolConfig.functionCallingConfig`; assistant `tool_calls` become
  `functionCall` parts, role `tool` messages become `functionResponse` parts,
  and both the non-streaming and streaming translators surface Gemini
  `functionCall` parts as OpenAI `tool_calls`. Chose the full mapping over the
  LM-2002 rejection. Covered by unit tests and wiremock round-trips in
  `crates/server/tests/chat.rs`.

## Noted while building M5

- **Accurate per-model tokenizer - DONE** (issue #8). ADR 003's opt-in
  "accurate tokenizer via `spawn_blocking`" now ships behind
  `[tokenizer] mode = "accurate"`: exact `tiktoken-rs` BPE (cl100k_base /
  o200k_base by model prefix) refines the local fallback AFTER the response
  is handed off (deferred accounting close, BPE on the blocking pool), landing
  in Prometheus and usage_log; the response envelope stays heuristic and the
  request is never delayed. Heuristic fallback for non-OpenAI models and any
  failure. The default stays the byte heuristic (zero cost). See the ADR 003
  "opt-in accurate tokenizer" addendum. Remaining follow-up: streaming input
  counts stay heuristic (no buffered prompt to BPE under the ADR 004
  passthrough rule).
- **Streaming output estimation = data-frame count.** When a stream carries no
  usage (rare: `include_usage` is auto-requested and translators always emit
  usage), the output-token estimate is the number of `data:` frames (~1 token
  per delta for OpenAI-style streams). Crude but honest (`estimated=true`).
- **usage_log records successful requests only.** ~~Refusals (401/402/429) and
  upstream failures are visible in logs/metrics but produce no usage row.~~
  RESOLVED (issue #26, ADR 007): admission refusals (402/429) now enqueue a
  status-only usage row (zero tokens, `status` carries the rejection) through
  the same non-blocking channel. 401 stays unlogged (refused in the auth
  middleware, before accounting opens - no key to attribute); upstream failures
  are already logged by the normal finish path.
- **Rejected requests still count toward RPM/TPM.** ~~Quota bumps are not
  unwound when a later admission step (budget) refuses the request.~~ RESOLVED
  (issue #26, ADR 007): a request refused *inside* admission (TPM after RPM, or
  the budget after both) now rolls back the bumps it already made, so it
  consumes no quota. (A request refused at its own step never bumped that
  window.)
- **DB-stored provider keys are boot-time only.** ~~`PUT /admin/provider-keys`
  takes effect at the next restart; wire it into the M7 hot-reload path.~~
  *Done (post-v0.1.0):* the admin route pings the hot-reload trigger and every
  reload re-reads provider keys from the encrypted store, so a rotation applies
  without a restart. Env-sourced keys keep precedence.
- **Metadata values are stringified** in `usage_log.metadata`. ~~JSON object of
  strings.~~ RESOLVED (issue #26, ADR 007): the column now stores typed JSON
  (`{"batch":42,"canary":true}`), so numeric/boolean filtering via SQLite
  `json_extract` works. Prometheus labels still stringify (labels are strings).
- **TPM debits the pre-call estimate, never adjusted.** ~~Unlike the budget
  (reserved then settled to real usage), the tokens-per-minute window keeps the
  estimate.~~ RESOLVED (issue #26, ADR 007): a successful request now settles
  the TPM window to the real token count, mirroring the budget - large
  `max_tokens` reservations no longer starve a key. A dropped (failed/cancelled)
  reservation deliberately keeps the TPM debit: a request that hit the gateway
  still counts against the rate limit.
- **No zeroization of key material.** `MasterKey` and the raw env string are
  not zeroized on drop; the `zeroize` crate would close the residual-memory
  window. Low risk (single long-lived process), noted from the M5 review.

## M6 (resilience) - deferred

- ~~**Per-provider connect timeout.**~~ **Done (issue #24, 2026-07-15.)** A
  provider may set `connect_timeout_ms`; it then gets its own (unpooled)
  `reqwest::Client` built at registry construction, while every non-overriding
  provider keeps sharing the one pooled client. See the ADR 005 amendment.
- ~~**First-frame-peek streaming retry.**~~ DONE (issue #7, 2026-07-15). The
  commitment point moved from the *open* phase to the first *content frame*: the
  streaming path now peeks the first upstream frame, so a stream that opens 200
  then errors or closes before any content frame fails over (and penalises the
  breaker) instead of committing to a terminal SSE error. Buffers at most one
  frame, bounded by `first_token`, cancellation-safe. See ADR 005, 2026-07-15
  amendment; `crates/router/src/peek.rs`.
- **Circuit-breaker map is unbounded by design.** One entry per (provider,
  model) actually seen; bounded by the configured surface, never by client
  input. If a future dynamic-model feature lets clients mint arbitrary model
  ids, add an LRU cap.
- **Health probe is a bare GET to `base_url` for keyed vendor kinds.** TEI,
  vLLM and Ollama now get a real, unauthenticated liveness endpoint (DEBT-3,
  issue #23). Keyed vendor kinds (OpenAI, Anthropic, the OpenAI-compatible
  hosts, ...) still get bare reachability only: their liveness routes
  (`GET /v1/models`, etc.) require an API key the probe task does not carry,
  and an unauthenticated call there would 401 a healthy server - a worse
  signal than bare reachability. Plumbing provider credentials into the probe
  task to unlock this is deferred to keep the probe free and side-effect-free.

- ~~**`health_stays_fast_under_upstream_429_storm` flaky under a 500-connection
  storm.**~~ **Resolved** (issue #27; the storm path's kernel-level connect
  retry landed via PR #42). Several dev sandboxes hit a client-side panic
  (`reqwest::Error` from a TCP connect reset or a broken-pipe `SendRequest`)
  when firing 500 concurrent requests at once - a saturated OS accept backlog,
  not gateway behaviour. `crates/server/tests/resilience.rs`'s `post_chat`
  helper now retries `is_connect()`/`is_request()` transport errors (never a
  received HTTP response) with a short backoff before giving up, and the storm
  size is overridable via `LUMEN_RESILIENCE_STORM_SIZE` (defaults to the
  unchanged CI-scale 500) as a secondary escape hatch. The status-code
  assertions (429/503 only, `/health` latency bound) are unchanged - the fix
  is purely about not letting host-level connection churn panic the test.

## M7 (release) - deferred

- **Hot reload swaps routing, pricing, resilience and the safe auth knobs.**
  SIGHUP / file-watch / admin-trigger re-validate the config and atomically swap
  the provider registry (ArcSwap), the price table, the resilience policy and the
  runtime-safe `[auth]` knobs (`flush_interval_ms`, `retention_days`). Still
  restart-only, by design: the **server bind address** (rebinding a live listener
  is high-risk), `auth.enabled`, `auth.db_path`, and the bounded usage-log
  channel knobs (`usage_channel_capacity`, `usage_batch_max`, `usage_flush_ms`)
  whose capacity is fixed when the channel is created.
- **Hot reload re-resolves env keys fresh; DB keys are re-read each reload.** A
  reload re-reads provider keys from the environment and, for env-keyless
  providers, from the encrypted DB store, so *rotating* a DB-stored key
  (`PUT /admin/provider-keys`) after boot takes effect on the next reload (the
  admin route triggers one) with no restart. A DB read error keeps the previous
  snapshot so a reload never strips a stored key.
- ~~**Anthropic/Gemini translation fuzzing** goes only as deep as the shared SSE
  parser today. Fuzzing the `translate_request`/`translate_response`/stream
  translators directly needs a small public (or `#[cfg(fuzzing)]`) shim over the
  currently-private functions.~~ **Resolved** (issue #27). Each of
  `providers::anthropic`/`providers::google` now has a `#[cfg(fuzzing)] pub mod
  fuzzing` shim (compiled only under `cargo fuzz`, which sets `--cfg fuzzing`
  across the dependency graph - zero normal-build surface change) exposing
  `translate_request`/`translate_response`. Four new targets
  (`anthropic_translate_request`, `anthropic_translate_response`,
  `google_translate_request`, `google_translate_response`) in `fuzz/`, wired
  into the weekly fuzz CI matrix; see `fuzz/README.md` "Why `#[cfg(fuzzing)]`
  shims" for the alternatives considered.
- ~~**Loaded throughput vs LiteLLM not measured in-repo.** The in-process overhead
  (~3 µs) is benchmarked; the full p50/p99/RAM/req·s head-to-head is a
  reproducible `bench/` harness (docker-compose + k6) run by the operator, not
  captured as a committed baseline.~~ **Resolved** (issue #27). `bench/run.sh`
  drives the full harness end to end (pinned, digest-locked images; one
  command) and writes a timestamped, committed result under `bench/results/`.
  A recorded baseline is linked from `docs/perf-baseline.md` - read that
  section's caveat before trusting the absolute numbers: it was recorded on a
  shared dev host, not dedicated hardware, so the *relative* LUMEN-vs-LiteLLM
  comparison is solid but the absolute figures are illustrative. Re-run
  `bench/run.sh` on real hardware for numbers to make capacity decisions on.

## Backlog debt paid down (post-v0.1.0)

- **Full-config hot reload (DEBT-1)** - done. Reload now swaps pricing and the
  resilience policy (retry/timeouts/fallbacks) as well as the routing table,
  preserving circuit-breaker state.
- **Auth-knob hot reload + DB provider-key rotation** - done. Reload also swaps
  the safe `[auth]` knobs (`flush_interval_ms`, `retention_days`) and re-reads
  DB-stored provider keys, and `PUT /admin/provider-keys` triggers a reload so a
  rotation applies without a restart. Server bind address stays boot-time (see
  the M7 note above for the exact restart-only surface).
- **Key-material zeroization (DEBT-2)** - done. `MasterKey` wipes on drop and
  the raw `LUMEN_MASTER_KEY` string is zeroized after use.
- **Richer health probe (DEBT-3, issue #23)** - done for the self-hosted,
  keyless kinds: TEI (`/health`), vLLM (`/health`) and Ollama
  (`/api/version`), each a real liveness endpoint (non-2xx = down). Keyed
  vendor kinds keep bare host-reachability (no reliable *unauthenticated*
  liveness endpoint); a per-kind, authenticated probe for vendor APIs remains
  out of scope (see the M6 entry above).

## Noted while building M8 (vision - image input to chat)

- **Per-image token heuristic for the estimation fallback - done** (issue #9).
  The estimation fallback (upstream reports no `usage`) now adds a flat
  per-image constant (`85` tokens for `"detail": "low"`, `765` for
  `"high"`/`"auto"`/unset) instead of counting an image part as `0`. A true
  per-dimension tile count (OpenAI's `85 + 170 * tiles`) still needs decoded
  pixel dimensions, which the gateway does not extract from a `data:` URI
  today and remains out of scope (no image-byte inspection on the request
  path). See the
  [ADR 003 addendum](adr/003-token-accounting.md#addendum-m8--vision--image-input).
- **Anthropic/Gemini file/GCS image URIs.** Only inline base64 (`data:` URIs)
  and, where the provider fetches it itself (Anthropic), remote `http(s)` URLs
  are supported. Anthropic's `source: {type: "file", file_id: ...}` and
  Gemini's GCS `fileUri` sources are not modelled; add if a caller needs
  pre-uploaded-file references instead of inline bytes.

## Provider coverage - next candidates (post-rename)

- **Tier-2 clouds need dedicated kinds** (different auth/schema, not
  OpenAI-compatible): ~~Azure OpenAI (deployment routing + api-version)~~
  (shipped - `kind = "azure"`), ~~AWS Bedrock (SigV4, per-model schemas)~~
  (shipped - `kind = "bedrock"`, Converse API + SigV4,
  `crates/providers/src/bedrock/`), ~~Google Vertex AI (GCP OAuth, regional
  endpoints)~~ (shipped - `kind = "vertex_ai"`,
  `crates/providers/src/google/vertex/`).
- ~~**Azure: dedicated `api_version` config field** - a desired fast-follow to
  the shipped `azure` kind, which currently reads the version from an
  `?api-version=...` query string on `base_url`. A first-class field needs a
  matching `ProviderSpec` + `crates/server/src/config.rs` change.~~
  **Resolved (issue #65).** The provider config now takes an optional
  `api_version` field (azure-only, rejected on other kinds at boot), threaded
  through `ProviderSpec` into `AzureProvider`. Precedence: the explicit field
  wins over an `?api-version=...` query string on `base_url` (kept for
  back-compat), which wins over the pinned built-in default.
- ~~**Cohere chat** (Command R/R+) - we ship Cohere embed+rerank; chat is a
  distinct schema.~~ **Resolved** (shipped -
  `crates/providers/src/cohere/chat.rs`, text chat + streaming + tools). The
  remaining slice is vision/image input only: the chat translation maps
  content with `image_url: None`, tracked as issue #73.
- **More rerankers**: ~~Mixedbread (mxbai-rerank)~~ (shipped -
  `kind = "mixedbread"`), ~~Pinecone Rerank~~ (shipped - `kind = "pinecone"`),
  ~~NVIDIA NIM rerank~~ (shipped - `kind = "nvidia"`), ~~Together LlamaRank~~
  (shipped - `kind = "together"`). All four live under
  `crates/providers/src/`; the section's "cheap differentiation for a
  first-class rerank gateway" goal is done.

## Noted while building M8 (vision / image input)

- **`LM-2004` pre-flight is primary-only.** A remote `http(s)` image URL is
  rejected up front (`LM-2004`, 400) only when the model's *primary* provider
  can't fetch it (Gemini). If the primary accepts URLs (OpenAI) but a Gemini
  model is a *fallback*, a fail-over to Gemini surfaces as `LM-3002` (502,
  translation error) rather than a client 4xx - safe (never fetched, no retry
  loop) but a soft break of the 4xx/5xx separation (rule 8). Options if it ever
  bites: scan the whole chain in the pre-flight (rejects some primary-servable
  requests), or add a dedicated client-input `ProviderError` mapped to a 4xx.
- **Per-image token heuristic for the estimation fallback - done** (issue #9,
  see the note above under "M8 (vision - image input to chat)"). The remaining
  gap is a true dimension-based tile count, which needs image decoding this
  gateway deliberately does not do on the request path.
- **Provider-native image URI forms.** Anthropic/Gemini file & GCS URI image
  sources (beyond inline base64 + remote URL) are not modelled.
- **Tool-role messages with image parts are silently flattened** (noted while
  fixing issue #73). The Cohere translator gates its v2 image blocks on the
  user role (Cohere's `ToolMessageV2` cannot carry images), so an image part
  on a `tool` message is flattened to its text - consistent with the
  Anthropic translator today, but honest handling would be a 400 before the
  upstream call. Candidate: a shared role-aware content check in the M8
  pre-flight instead of per-translator conventions.
- **Remote image URLs on OpenAI-compatible-path providers whose upstream only
  accepts base64** (noted while wiring ollama chat, issue #63). Every kind
  served by the shared OpenAI provider inherits
  `accepts_remote_image_url() = true`, including `ollama` (its `/v1` chat
  endpoint takes base64 `data:` URIs only). So a remote `http(s)` image URL on
  a vision-declared ollama model skips the `LM-2004` pre-flight and fails
  opaquely upstream instead of as an honest client 4xx. Candidate fix: gate
  `accepts_remote_image_url()` on the provider kind (or a per-spec flag)
  rather than on the implementing provider type.
