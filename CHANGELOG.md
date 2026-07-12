# Changelog

All notable changes to Ferrogate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
