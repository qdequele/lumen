# Backlog

Ideas surfaced during development that are intentionally out of scope for v1
(see `CLAUDE.md` → "Ce qu'on ne fait PAS (v1)" and `ROADMAP.md` → "Backlog v2").
Recorded here so they are not lost, and so we don't gold-plate the current
milestone.

## Deferred to v2 (from the vision)

- Web admin UI
- Semantic cache
- Multimodal (images / audio) support
- Guardrails / moderation
- Distributed rate limiting (Redis)
- OTLP tracing export
- WASM plugin system

## Noted while building M1

- Token-array inputs for `/v1/embeddings` (`input` as arrays of token ids) are
  not modelled — only string and string-batch. Add if a provider needs it.
- Rerank `documents` accepts only strings; Cohere also allows objects. Reduce
  object documents to text at the edge when a provider requires it.
- Config: consider a `--check-config` subcommand that validates and exits, for
  CI / deploy pipelines, once the CLI surface grows.
- Error taxonomy (revisit in M4): `ProviderError::Cancelled` currently maps to
  `GatewayError::Internal` (500 / `internal`). Once real streaming/provider
  calls exist, a client-initiated cancel should not inflate `internal` metrics —
  consider a dedicated non-5xx variant that isn't alerted on.
- `error_type()` collapses 401/402/429 into `invalid_request` because the public
  taxonomy only has three `type`s. Fine per `CLAUDE.md`, but note it's coarse.
- Acceptance criterion "boot < 100 ms" is verified manually (M1); fold a real
  timing assertion into the M7 criterion benchmarks rather than a flaky unit test.
- Graceful shutdown is unit-tested via an injected shutdown future; the real
  SIGINT/SIGTERM path (`shutdown_signal`) has no integration test (hard to do
  portably). Acceptable; revisit if signal handling grows.

## Noted while building M2

- Embedding output is always a float array in v1. Base64 embeddings are decoded
  on the way IN (a client requesting `encoding_format: "base64"` won't error),
  but we do not re-encode on the way OUT. Add base64 *output* if a client needs it.
- Ollama drops the OpenAI-only `dimensions` field with a `debug!` log; a client
  asking for a specific dimension silently gets full-width vectors. Consider a
  400 (FG-1001) when an unsupported-but-meaningful field is set under a strict mode.
- `FG-1002` (payload too large, 413) is emitted by `RequestBodyLimitLayer` as a
  raw 413 without our JSON error envelope. Map the tower-http rejection to
  `GatewayError::PayloadTooLarge` for a consistent body.
- Cancellation tests use real (short) wall-clock delays rather than
  `tokio(start_paused)`; robust today but revisit if they flake under CI load.
  The HTTP-level disconnect test asserts the server stays responsive and the
  upstream got the request — the actual upstream abort is proven at the provider
  layer (conformance `scenario_cancellation_aborts_upstream`).
