# Changelog

All notable changes to LUMEN are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
