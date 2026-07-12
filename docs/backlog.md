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
