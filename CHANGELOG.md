# Changelog

All notable changes to Ferrogate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — M6: résilience (retries, fallback, circuit breaker, timeouts, health)

**M6 is complete.** The gateway now survives flaky upstreams without becoming
flaky itself: retries, multi-provider fallback, a per-provider circuit breaker,
per-phase timeouts and optional background health checks — none of it on a
database path, and `/health` stays independent of provider health (the LiteLLM
#15526 lesson: a 429 storm must not destabilise the gateway).

- **Retries** (`[resilience]`): retryable upstream failures only (5xx,
  connect/read timeout, 429 — *never* a client 4xx) with exponential backoff +
  equal jitter (`retry_base_ms` 200, `retry_max_ms` 5000, `retry_max_attempts`
  3), honouring an upstream `Retry-After` as a floor. The backoff maths are a
  pure, unit-tested function fed a lock-free splitmix64 fraction — no
  dependency, no blocking, no clock read on the hot path.
- **Fallback chains** (§6.2): per-model `fallbacks = ["model-b", …]`, tried in
  order once the primary's retries are spent or its circuit is open. Each
  fallback must exist and serve every capability of the model it backs
  (validated at boot). Responses carry `x-ferrogate-model-used` and
  `usage_log.model_used` records which model actually served; metrics and cost
  attribute to the served model.
- **Circuit breaker** (§6.3): per (provider, model), Closed → Open (after
  `circuit_failure_threshold` 5 consecutive faults) → Half-Open (after
  `circuit_cooldown_ms` 30 000, exactly one probe) → Closed/Open. In-memory,
  the lock never held across an await; state exported as
  `ferrogate_circuit_state{provider,model}` (0/1/2). Open with no fallback left
  → 503 `FG-3020` with `Retry-After`.
- **Timeouts** (§6.4): `connect_timeout_ms` (5000, client-wide → `FG-3012`),
  `first_token` (the M4 `server.first_token_timeout_ms`, 30 000 → `FG-3011`) and
  `total_timeout_ms` (600 000 → `FG-3013`), each a distinct code. `first_token`
  and `total` are overridable per provider; `connect` is client-wide (one
  pooled HTTP client). All bounded by the executor's absolute total deadline.
- **Streaming stays committed once it starts**: retry/fallback happen only while
  *opening* the upstream byte stream; after the first frame the M4 guards
  (FG-3010/3011, heartbeat) own the stream and never retry.
- **Background health checks** (§6.5, `health_check_enabled` off by default):
  a periodic probe of every provider with a configured `base_url` fills
  `GET /health/providers` and `ferrogate_provider_up{provider}`; vendor-default
  URLs report `unknown` (never probed). Entirely off the request path.
- **Errors**: `FG-3012` (connect timeout, 504), `FG-3013` (total timeout, 504),
  `FG-3020` (circuit open, 503). `ProviderError` gained `is_retryable` /
  `is_provider_fault` classification shared by the retry loop and the breaker.
- **ADR 005** records the execution model (one generic chain executor,
  capability-specific resolution, breaker placement, streaming boundary, the
  connect-per-provider and first-frame-peek simplifications deferred).

### Added — M5: auth, virtual keys, hard budgets & token accounting

**M5 is complete.** The gateway can now be shared safely: keys, budgets that
can NEVER be overrun, and a token count for every single request.

- **Virtual keys** (`[auth]`, off by default): `fg-` + 32 random bytes,
  stored as a BLAKE3 hash only (the keys are 256-bit random — a password KDF
  would just burn hot-path CPU). Auth on all of `/v1/*`; unknown, disabled
  and expired keys are one indistinguishable `FG-4004` (401). `/health` and
  `/metrics` stay open.
- **Hard budgets, enforced in memory** (M5 §5.2): the pre-call cost estimate
  is *reserved* with an atomic CAS before the upstream call and settled to
  the real usage after it — 50 concurrent requests against a budget for 10
  admit exactly 10 (tested at both the atomic and the HTTP level). Refusals
  happen BEFORE any upstream traffic: 402 `FG-4001` (budget), 429 `FG-4002`
  (RPM) / `FG-4003` (TPM, with `Retry-After`). The DB is never consulted on
  the request path; budgets flush to SQLite periodically (default 10 s — a
  crash loses at most that much *accounting*, never allows an overrun) and
  reload at boot, so an exhausted key stays exhausted across restarts.
- **Token accounting always on** (ADR 003): every chat/embed/rerank call
  yields a count — upstream usage when reported (`estimated=false`), else a
  byte-heuristic estimate flagged `estimated: true` in the response body, in
  `ferrogate_tokens_total{capability,model,provider,direction,estimated}`
  and in `usage_log`. TEI's report-nothing embeddings now count > 0.
  Streaming chat sniffs the final usage chunk with bounded state (no
  response accumulation); rerank counts search units (upstream-billed or
  derived, never silently zero). The opt-in accurate tokenizer stays in the
  backlog — only the O(bytes) heuristic runs, inline and hot-path-safe.
- **Cost counting** (§5.4b): per-model prices in config (`cost_per_1m_input`,
  `cost_per_1m_output`, `cost_per_1k_searches`) feed both the budget
  reservation and the `usage_log.cost` column. Unpriced models cost 0.
- **Async usage log** (§5.3): bounded mpsc (default 10 000) → batched writer
  (500 entries / 2 s); a full channel drops the entry and bumps
  `ferrogate_usage_log_dropped_total` — the request path NEVER blocks on
  logging. Background retention purge (default 30 days). No prompt/response
  content is ever stored.
- **Request metadata** (ADR 002): `x-ferrogate-metadata` (alias
  `cf-aig-metadata`), a flat JSON object bounded at 16 keys / 64 B keys /
  256 B values / 4 KiB, parsed once at the edge. Full object → structured
  logs + `usage_log.metadata`; ONLY `telemetry.metadata_labels` allowlist
  keys become Prometheus labels (default empty — client metadata can never
  mint a time series). Malformed metadata never fails the request: dropped
  with a warn + `ferrogate_metadata_rejected_total`.
- **Admin API** (§5.5), mounted only when auth is on and gated by
  `FERROGATE_MASTER_KEY` (64 hex chars, compared by hash): `POST/GET
  /admin/keys`, `PATCH /admin/keys/{id}` (changes apply immediately, no
  restart), and `PUT /admin/provider-keys/{name}` to store provider keys
  AES-256-GCM-encrypted at rest (env vars remain the default; DB keys
  back-fill env-less providers at boot). Key creation is the single moment
  the plaintext exists.

### Changed

- **FG-4xxx codes realigned to the M5 spec** (they were placeholders, never
  emitted): `FG-4001` = budget exhausted (402), `FG-4002` = RPM (429),
  `FG-4003` = TPM (429), `FG-4004` = missing/invalid key (401).
- `Usage`, `EmbedUsage` and `RerankUsage` gained an optional `estimated`
  field, omitted unless the gateway estimated the counts (ADR 003).

### Added — M4 (final slice): streaming translation, tools, and stream guards

**M4 is complete.** This slice closes every remaining criterion:

- **Incremental SSE parser** (`providers::sse`): reassembles upstream events
  fragmented across TCP packets (LF and CRLF, multi-line `data:`, comments
  ignored), buffering only the current incomplete event with a hard size cap.
- **Anthropic streaming translation**: typed events (`message_start`,
  `content_block_start/delta`, `message_delta`, `message_stop`) → OpenAI
  chunks, including streamed **tool_use** (`input_json_delta` → `tool_calls`
  argument deltas, OpenAI indices allocated in order of appearance). Bounded
  state — the response text is never accumulated. In-stream `error` events
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
  - *first-token timeout* (`first_token_timeout_ms`, default 30 s) → FG-3011:
    a plain 504 when the upstream never answered, an SSE error frame when the
    stream had started; non-streaming applies the window to the whole upstream
    call (per-phase timeouts land in M6);
  - *missing terminator* → FG-3010 error frame when the upstream dies without
    `data: [DONE]` (criterion 5) — detection survives a `[DONE]` split across
    frame boundaries; the gateway never fabricates the terminator itself;
  - *heartbeat* (`sse_heartbeat_ms`, default 15 s): `: ping` comments on idle
    streams so proxies don't reap slow upstreams.
- **Streaming usage (ADR 003), upstream half**: passthrough requests
  `stream_options.include_usage`; translated providers emit full usage in the
  final chunk. The local-estimation fallback (`estimated=true`) moves to M5
  with the Prometheus counters and `usage_log`.

### Added — M4 (slice 3, partial): Google Gemini + Mistral embeddings

- **Google Gemini** chat provider (non-streaming) with bidirectional
  translation: OpenAI messages → `contents` (assistant→`model`, system hoisted
  to `systemInstruction`), params → `generationConfig`, response `candidates`/
  `finishReason`/`usageMetadata` → OpenAI shape. Auth via `x-goog-api-key`
  header; the model rides in the URL path, the key never does.
- **Mistral embeddings** (`EmbeddingProvider`, OpenAI-compatible passthrough) —
  Mistral now serves both chat and embeddings; added to the embeddings
  conformance suite.

  *Still remaining to complete M4:* Anthropic + Gemini streaming-event
  translation (criterion 4), first-token timeout FG-3011 (criterion 6),
  upstream-closes-without-`[DONE]` → FG-3010 (criterion 5), SSE heartbeat, and
  streaming token estimation (ADR 003).

### Added — M4 (slice 2): zero-copy SSE streaming

- Real incremental streaming for `stream=true`: the gateway forwards the
  upstream SSE bytes **verbatim** — no per-chunk `serde` round trip (ADR 004).
  New `ChatProvider::chat_stream_bytes` (default serializes the typed
  `chat_stream`; OpenAI/Mistral override it to pipe `reqwest`'s `bytes_stream`
  via the shared `http::open_stream`). The server writes a raw `Bytes` body
  (`Body::from_stream`, `content-type: text/event-stream`) with the cancel
  drop-guard moved inside it, so a client disconnect aborts the upstream.
  Proven byte-identical over 100 chunks; `stream_options.include_usage` is
  requested automatically without overriding a client's choice.

  *Still deferred to slice 3:* Anthropic streaming-event translation, Google
  Gemini, Mistral embeddings, first-token timeout (FG-3011), SSE heartbeat, and
  streaming token sniffing/estimation (ADR 003).

### Added — M4 (slice 1): chat completions (non-streaming)

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
- Chat routing (`resolve_chat`, FG-2001/FG-2002) and registry chat routes; a
  shared `chat::single_shot_stream` adapter backs the interim `chat_stream`.
- Reserved streaming error codes `FG-3010` (upstream stream interrupted, 502)
  and `FG-3011` (first-token timeout, 504) in the taxonomy and `docs/errors.md`.

  *Deferred to the M4 streaming slice:* zero-copy incremental SSE passthrough,
  Anthropic streaming-event translation, Google Gemini, Mistral embeddings, the
  first-token timeout, and streaming token estimation (ADR 003).

### Changed

- `http::post_json` gained a header-based sibling `post_json_with_headers` (for
  Anthropic's non-bearer auth); the two share one send/classify core.

### Added — M3: reranking & model discovery

- `POST /v1/rerank` (Cohere wire format): `documents` accept bare strings or
  `{ "text": ... }` objects; the gateway guarantees the client-facing invariants
  regardless of upstream behaviour — results sorted by descending
  `relevance_score`, `top_n` clamped to the document count then truncated,
  `document` echoed only when `return_documents` is set (off by default). Empty
  `documents` is rejected with `FG-2010` (400) before any upstream call.
- Four new providers, each implementing **both** `EmbeddingProvider` and
  `RerankProvider`: **Cohere** (v2 `embed`/`rerank`), **Jina**
  (OpenAI-compatible embed, Cohere-shaped rerank), **TEI** (self-hosted, keyless,
  bare-array `/embed` and `/rerank`), and **Voyage** (`top_k`/`data[]` rerank).
- A generic **rerank conformance suite** all four providers pass identically
  (ordering, 429/`Retry-After`, 5xx, malformed response, cancellation) — the
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
  **1.97.0** (from 1.95.0) — clippy pedantic and the full suite stay green.
- Planned a Cloudflare-style per-request metadata header
  (`x-ferrogate-metadata`) for logs, `usage_log` and cardinality-bounded
  Prometheus labels — design in ADR 002, tasks folded into the M5 spec.
- Elevated **token accounting** to a first-class, always-on promise: every
  request of every capability yields a token count (upstream usage when present,
  else a labelled local estimate — never a silent zero, e.g. TEI), surfaced in
  the response, Prometheus counters and `usage_log`. Design in ADR 003; tasks
  threaded through M4 (streaming extraction) and M5 (counters, estimation,
  storage), and added to the mission pillars and ROADMAP.

### Changed

- Extracted a shared `http::post_json` helper (transport + error classification)
  that every provider now shares, including OpenAI and Ollama (behaviour
  unchanged); only body translation differs per provider.
- Added `FG-2010` (empty rerank `documents`, 400) to the taxonomy and
  `docs/errors.md`, and a `Voyage` variant to `ProviderKind`.

### Added — M2: embeddings (first complete request path)

- `POST /v1/embeddings` end to end (OpenAI wire format): validate → route →
  provider → response, with the client model id resolved to its upstream alias.
- OpenAI embeddings provider (the canonical reference) and a keyless Ollama
  provider, both driven by a shared, pooled rustls HTTP client.
- A generic embeddings **conformance suite** that both providers pass
  identically (nominal, batching-in-order, 429/`Retry-After`, 5xx, malformed
  response, cancellation) — the reusable harness every future provider must pass.
- Provider **registry** behind `ArcSwap` (ready for M7 hot reload) that builds
  instances from config-derived specs and resolves `(capability, model)`; the
  **router** turns misses into `FG-2001` (unknown model, 404) or `FG-2002`
  (capability mismatch, 400).
- Automatic **batching**: requests over a provider's `max_batch_size` split into
  sub-batches run with bounded concurrency (default 4), reassembled in original
  order with summed usage; any sub-batch failure fails the whole request.
- End-to-end **cancellation**: a per-request `CancellationToken` is cancelled on
  client disconnect and aborts the in-flight upstream call.

### Changed

- Error taxonomy realigned to the codes pinned by the M2 spec: `1xxx` request,
  `2xxx` routing (`FG-2001`/`FG-2002`), `3xxx` upstream (`FG-3001` rate-limited,
  `FG-3002` malformed-response → 502, plus generic/unavailable/timeout), `4xxx`
  auth/budget, `5xxx` internal. Added a `ProviderError::Unavailable` variant for
  transport failures (→ 503). `docs/errors.md` updated.
- `ProviderKind` moved from the server config into the `providers` crate (it is
  the registry's construction discriminant); crate package names stay bare.

### Added — M1: skeleton & foundations

- Cargo workspace with six crates (`core`, `providers`, `router`, `auth`,
  `telemetry`, `server`), release profile tuned for a small, fast binary
  (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip`), pinned
  stable toolchain, and Apache-2.0 license.
- `core`: capability traits (`ChatProvider`, `EmbeddingProvider`,
  `RerankProvider`) taking a `CancellationToken`; OpenAI-shaped chat/embeddings
  types and Cohere-shaped rerank types (unknown fields preserved for
  passthrough); the `Capability` enum; and the two-layer error taxonomy
  (`ProviderError` → `GatewayError`) with stable `FG-XXXX` codes and a standard
  JSON error envelope.
- `telemetry`: a Prometheus registry wrapper and structured-logging setup.
- `server`: axum binary with `GET /health` (no I/O, always 200 while alive) and
  `GET /metrics`; per-request `x-request-id`, tracing spans (metadata only —
  never body or query string), and a configurable body-size limit; bounded
  graceful shutdown on SIGINT/SIGTERM (30 s drain).
- Configuration via figment (TOML + `FERROGATE_*` env overrides) with
  boot-time validation that exits non-zero naming the offending field; API keys
  are referenced by env-var name only, never stored. Commented
  `config.example.toml`.
- GitHub Actions CI: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -D warnings` (pedantic), and `cargo test --workspace`.
- Docs: error-code reference (`docs/errors.md`), ADR 001 (crate/lib naming),
  and this changelog.

[Unreleased]: https://github.com/meilisearch/ferrogate/commits/main
