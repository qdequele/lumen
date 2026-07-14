# ROADMAP — LUMEN

> Instruction for Claude Code: process the milestones IN ORDER. The current milestone = first unchecked. Read its spec in `specs/milestones/` before any code. Check the boxes here AND in the spec as you go. Never start a milestone if the previous one has red tests.

> **Cross-cutting promise — token counting (ADR 003):** EVERY request of EVERY capability (chat/embed/rerank) produces a token count, never zero by default: upstream usage if present, otherwise a local estimate marked `estimated`. Exposed in the response, in Prometheus counters, and in `usage_log` (M5). It is a central reason for being of the project.

## M1 — Skeleton & foundations ✅
- [x] Cargo workspace, 6 crates (core, providers, router, auth, telemetry, server)
- [x] Capability types and traits in core (ChatProvider, EmbeddingProvider, RerankProvider)
- [x] axum server: /health, /metrics (stub), graceful shutdown
- [x] figment config (TOML + env) + config.example.toml
- [x] LM-XXXX error taxonomy + standard JSON error response
- [x] GitHub Actions CI: fmt + clippy -D warnings + tests
Spec: `specs/milestones/M1-skeleton.md`

## M2 — Embeddings (first complete path) ✅
- [x] POST /v1/embeddings OpenAI format
- [x] OpenAI embeddings provider + Ollama embeddings provider
- [x] Automatic batching (splitting by max_batch_size, ordered reassembly)
- [x] Router: (capability, model) → provider resolution from the config
- [x] End-to-end cancellation tested
Spec: `specs/milestones/M2-embeddings.md`

## M3 — Reranking + model discovery ✅
- [x] POST /v1/rerank Cohere format
- [x] Providers: Cohere (embed+rerank), Jina (embed+rerank), TEI self-hosted (embed+rerank), Voyage (embed+rerank)
- [x] GET /v1/models with capabilities per model
- [x] Versioned model aliasing in the config (IDs belong to the user alone)
Spec: `specs/milestones/M3-rerank-models.md`

## M4 — Chat + SSE streaming
- [x] POST /v1/chat/completions non-streaming, OpenAI format
- [x] Zero-copy SSE streaming (Bytes passthrough when the schema is identical)
- [x] Anthropic provider with bidirectional translation (messages, system, tool_use, usage)
- [x] Mistral + Google (Gemini) providers, streaming included
- [x] Client disconnect → upstream abort, tested
- [x] Stream guards: first-token timeout (LM-3011), upstream dead without `[DONE]` (LM-3010), heartbeat `: ping`

Note: local token estimation in streaming (upstream usage absent →
`estimated=true`, ADR 003) ships in M5 along with the Prometheus counters and
`usage_log`; upstream usage itself is already propagated (last chunk).
Spec: `specs/milestones/M4-chat-streaming.md`

## M5 — Auth, virtual keys & hard budgets ✅
- [x] SQLite (sqlx): hashed virtual keys, provider keys encrypted at rest (AES-GCM)
- [x] HARD budgets per key, enforced IN the request path before the upstream call
- [x] RPM/TPM quotas per key
- [x] Cost counting per capability (chat tokens, embeddings input tokens, rerank searches)
- [x] Usage log writes via bounded channel → batched writer (never sync)
- [x] Local token estimation when the upstream returns none (streaming included), marked `estimated` (ADR 003)
- [x] Per-request metadata header (`x-lumen-metadata`, Cloudflare AI Gateway style) → logs + `usage_log` + Prometheus labels via allowlist (ADR 002)

Note: local estimation = byte heuristic (inline, hot-path-safe);
the precise opt-in tokenizer (spawn_blocking) ships in the backlog — see
`docs/backlog.md` § M5.
Spec: `specs/milestones/M5-auth-budgets.md`

## M6 — Resilience ✅
- [x] Retries with backoff + jitter (honors Retry-After)
- [x] Multi-provider fallback chains per model
- [x] Circuit breaker per provider
- [x] Configurable timeouts (connect, first-token, total)
- [x] Provider health checks in the background, NEVER in the request path
Spec: `specs/milestones/M6-resilience.md`

## M7 — Release ✅
- [x] criterion benchmarks + public comparison vs LiteLLM (added latency, RAM, throughput)
- [x] Multi-arch distroless Dockerfile < 20 MB, static musl binary
- [x] Config hot reload without dropping connections
- [x] Complete docs (README, quickstart, provider guides, errors.md)
- [x] cargo-audit + cargo-deny in CI
Spec: `specs/milestones/M7-release.md`

Note: off-network overhead measured (~3 µs median, 10.6 MB image, idle RSS
8.8 MB); the loaded comparison vs LiteLLM is provided as a reproducible harness
(`bench/`) — see `docs/perf-baseline.md`. cargo-audit/deny/fuzz wired into CI
(binaries not installed in the dev environment). amd64 image via buildx CI;
arm64 verified locally (`docker run`).

## M8 — Vision (image input to chat) ✅
- [x] Core types: `MessageContent`/`ContentPart`/`ImageUrl` (`content` is a string OR an array of parts), `text()`/`has_image()`
- [x] Per-model `modalities` config (default `["text"]`), exposed in `GET /v1/models`
- [x] Pre-flight enforcement: an image to a non-vision model → `LM-2003` (400), before any upstream call
- [x] Anthropic translation (`image` base64/url blocks) and Gemini (`inline_data`); a remote URL to Gemini → `LM-2004` (400), the gateway never fetches upstream
- [x] OpenAI-family (+ `vllm`): verbatim passthrough of parts, conformance-tested
- [x] Configurable request body-size limit → `LM-1002` (413) envelope on all routes
- [x] Token counting (ADR 003): upstream usage is authoritative; text-only estimation fallback (image = 0), always `estimated`

Note: a per-image token heuristic (OpenAI tile formula) and file/GCS URIs
(Anthropic/Gemini) are deferred — see `docs/backlog.md`.
Spec: `docs/superpowers/specs/2026-07-14-vision-image-input-design.md`.

## Backlog v2 (do not implement)
Admin UI, semantic cache, audio (STT/TTS), image generation/output, guardrails, distributed rate limiting (Redis), OTLP tracing, WASM plugin.

Note: M8 shipped the first, narrowest slice of the "multimodal (images/audio)"
non-goal — image input to chat only (see above). Image output and audio (input
and output) remain out of scope.
