# LUMEN - Universal LLM Gateway in Rust

## Mission
A self-hostable, lightweight and fast gateway for **all types of models**: chat/LLM, embeddings, reranking. An alternative to LiteLLM (too heavy, Python, 1.7-4x overhead) and OpenRouter (SaaS, not self-hostable, telemetry).

## Non-negotiable pillars (in order)
1. **Performance**: < 1 ms added latency p99, zero-copy streaming, ~15 MB RAM idle.
2. **Sovereignty**: zero telemetry, prompts NEVER logged by default, single self-host binary.
3. **Robustness**: propagated cancellation, backpressure, DB off the request path.
4. **Multi-capability**: chat + embeddings + rerank are first-class citizens.
5. **Token observability**: EVERY request of EVERY capability produces a token count (never zero by default) - upstream usage if available, otherwise a local estimate marked `estimated`. Exposed in the response, in Prometheus, and in `usage_log`. A central reason for being. See ADR 003.

## Architecture (Cargo workspace)
```
crates/
├── core        # shared types, Provider traits, errors (thiserror)
├── providers   # 1 module per provider (openai, anthropic, cohere, ollama, tei...)
├── router      # model→provider resolution, fallback, load balancing
├── auth        # virtual keys, quotas, hard budgets
├── telemetry   # Prometheus metrics, structured logs (tracing), TOKEN counting (ADR 003) + costs
└── server      # axum binary, SSE, config, hot reload
```

### Capability traits (crates/core)
```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<ChatResponse, ProviderError>;
    async fn chat_stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError>;
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, req: EmbedRequest, cancel: CancellationToken)
        -> Result<EmbedResponse, ProviderError>;
    fn max_batch_size(&self) -> usize;
}

#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, req: RerankRequest, cancel: CancellationToken)
        -> Result<RerankResponse, ProviderError>;
}
```
A provider implements 1 to N traits. The router routes by (capability, model).

### Public API
- `POST /v1/chat/completions` - OpenAI format, SSE streaming
- `POST /v1/embeddings` - OpenAI format
- `POST /v1/rerank` - Cohere format (`query`, `documents`, `top_n`)
- `GET /v1/models` - exposes `"capabilities": ["chat"|"embed"|"rerank"]` per model
- `GET /health` - isolated path, touches NEITHER the DB NOR the providers
- `GET /metrics` - Prometheus

## Mandated stack
- **Runtime**: tokio (multi-thread), axum, tower, hyper
- **HTTP client**: reqwest (rustls, NOT openssl)
- **Serialization**: serde + serde_json
- **DB**: sqlx + SQLite by default; Postgres behind the `postgres` feature flag
- **Errors**: thiserror in libs, anyhow ONLY in main.rs
- **Logs**: tracing + tracing-subscriber (JSON in prod)
- **Config**: figment (TOML + env vars), hot reload via notify
- **Tests**: wiremock to mock providers, tokio::test

## STRICT code rules
1. **FORBIDDEN**: `unwrap()`, `expect()`, `panic!()` outside tests and main.rs (justify with a comment if an exception).
2. **FORBIDDEN**: blocking the tokio runtime (no `std::thread::sleep`, no sync I/O).
3. **MANDATORY**: every provider call takes a `CancellationToken`; dropping the HTTP client = abort of the upstream request (lesson from LiteLLM issue #22805).
4. **MANDATORY**: request logging goes through a bounded mpsc channel → async batched writer. NEVER a synchronous DB write in the request path (lesson from LiteLLM issue #12067).
5. **MANDATORY**: provider secrets are NEVER logged, never in errors returned to the client, never in Debug (custom `#[derive]` or the `secrecy` crate).
6. Clippy pedantic enabled: `cargo clippy --workspace --all-targets -- -D warnings` must pass.
7. Every public module has a doc comment. Every error has a stable code (`LM-1001` etc.) documented in `docs/errors.md`.
8. Errors ALWAYS distinguish: client error (4xx) / upstream provider error (502/503 + provider name) / internal gateway error (500). Never a misleading 401 during an internal outage (lesson from OpenRouter).
9. **FORBIDDEN**: em-dashes (the U+2014 character) anywhere in the repo, in source, docs, config or commit messages. Use hyphens, commas, colons, parentheses, or restructure the sentence. CI rejects em-dashes (`no-em-dashes` job).

## Work loop (to follow every session)
0. **Dependency freshness (ALWAYS, at the start of a session)**: `rustup update`
   for the latest stable, then `cargo outdated --workspace --root-deps-only`.
   Bump the versions in `Cargo.toml` when it is safe, then re-run the
   validation (step 4) - clippy pedantic may introduce new lints with
   each Rust version. Note any notable bump in `CHANGELOG.md`.
1. Read `ROADMAP.md` → identify the current milestone (first unchecked).
2. Read the corresponding `specs/milestones/M<N>-*.md`.
3. For each task of the milestone: write the tests FIRST, then the implementation.
4. Validate: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.
5. Check the boxes in ROADMAP.md and the milestone file. Add an entry in `CHANGELOG.md`.
6. Atomic commit per task: `feat(router): fallback chain with circuit breaker`.
7. If an architecture choice is not covered by the specs: write a short ADR in `docs/adr/NNN-titre.md` BEFORE implementing.

## Definition of Done (per task)
- [ ] Unit tests + at least 1 integration test (wiremock)
- [ ] No clippy warning
- [ ] Cancellation tested if the task touches the request path
- [ ] No secret in the logs (verify with a test)
- [ ] Doc comments on the public API

## Commands
```bash
cargo test --workspace                # tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo run -p server -- --config config.example.toml
cargo bench                           # benchmarks criterion (M7)
```

## Available subagents (.claude/agents/)
- `provider-integrator`: adds a new provider (repeatable pattern)
- `test-writer`: writes the tests before the implementation
- `code-reviewer`: read-only review after each milestone
- `perf-auditor`: tracks allocations, copies, runtime blocking
- `docs-writer`: user docs, README, config examples

## Issue labels (when triaging or filing GitHub issues)
Classify along four independent axes. Apply one label from each relevant axis;
`scope:` may be multiple or omitted. Full reference in `CONTRIBUTING.md`.
- **Type**: `bug`, `enhancement`, `documentation`, `question` (+ `good first issue`, `help wanted`, `duplicate`/`invalid`/`wontfix`).
- **`priority:`**: `high` (correctness/spec gap or high-demand) / `medium` / `low`.
- **`area:`** (subsystem): `providers`, `streaming`, `tokenizer`, `observability`, `config`, `resilience`, `testing`, `vision`.
- **`scope:`** (capability): `chat`, `embedding`, `reranking`. Apply one or more; omit for cross-cutting infra (config, resilience, tokenizer, testing, observability) not tied to a single capability.

## What we do NOT do (v1)
Web UI, billing, semantic cache, guardrails/moderation, image/audio support, plugin system. Note the ideas in `docs/backlog.md` and move on.
