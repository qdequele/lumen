# Contributing to LUMEN

Thanks for your interest in LUMEN — a lightweight, fast, self-hostable gateway
for **chat/LLM, embeddings and reranking**. This guide covers how to set up,
what the bar is, and how we label and land work.

Before writing code, skim the [README](README.md) for what LUMEN is and the
[`CLAUDE.md`](CLAUDE.md) for the project's non-negotiable pillars and strict
code rules — they are the contract, not suggestions.

## Ground rules

The four pillars, in priority order, decide every trade-off:

1. **Performance** — < 1 ms added latency p99, zero-copy streaming, ~15 MB RAM idle.
2. **Sovereignty** — zero telemetry, prompts never logged by default, single binary.
3. **Robustness** — propagated cancellation, backpressure, DB off the request path.
4. **Multi-capability** — chat + embeddings + rerank are first-class citizens.

Hard code rules (full list in [`CLAUDE.md`](CLAUDE.md#strict-code-rules)):

- No `unwrap()` / `expect()` / `panic!()` outside tests and `main.rs`.
- Never block the tokio runtime (no sync I/O, no `std::thread::sleep`).
- Every provider call takes a `CancellationToken`.
- No synchronous DB write in the request path — logging goes through a bounded
  channel to an async batched writer.
- Provider secrets are never logged, never in errors, never in `Debug`.
- `thiserror` in libraries, `anyhow` only in `main.rs`.
- Every error has a stable code (`LM-1001`, …) documented in [`docs/errors.md`](docs/errors.md).

## Development setup

```bash
# Toolchain is pinned via rust-toolchain.toml; rustup will honor it.
rustup update

cargo build --workspace
cargo run -p server -- --config config.example.toml   # run locally
```

## Validation — must pass before you open a PR

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # clippy pedantic
cargo fmt --check
```

Definition of Done for any change touching source:

- [ ] Unit tests + at least one integration test (wiremock for providers)
- [ ] No clippy warning
- [ ] Cancellation tested if the change touches the request path
- [ ] A test proving no secret leaks into logs, if secrets are involved
- [ ] Doc comments on any new public API

## Commits & pull requests

- **Conventional commits**, scoped and atomic — one logical change per commit:
  `feat(router): fallback chain with circuit breaker`,
  `fix(providers): reject remote image URLs for Gemini`,
  `docs(errors): document LM-3010`.
- Keep the PR focused. Note any user-visible change in [`CHANGELOG.md`](CHANGELOG.md).
- If you make an architecture choice not covered by the specs, write a short ADR
  in [`docs/adr/NNN-title.md`](docs/adr/) **before** implementing.
- Adding a provider? Follow the repeatable pattern — capability traits, schema
  translation, streaming, cancellation, and wiremock tests. See
  [`docs/providers.md`](docs/providers.md).

## Issue & PR labels

Issues are classified along four independent axes. An issue usually carries one
label from each relevant axis; cross-cutting infrastructure may have no `scope:`.

### Type — what kind of work

| Label | Meaning |
|-------|---------|
| `bug` | Something isn't working. |
| `enhancement` | New feature or request. |
| `documentation` | Docs improvements or additions. |
| `question` | Further information is requested. |
| `good first issue` | Good for newcomers. |
| `help wanted` | Extra attention is needed. |
| `duplicate` / `invalid` / `wontfix` | Triage outcomes. |

### `priority:` — how urgent

| Label | Meaning |
|-------|---------|
| `priority: high` | Correctness/spec gap or high-demand feature. |
| `priority: medium` | Useful robustness or feature work. |
| `priority: low` | Nice-to-have / edge case. |

### `area:` — which subsystem

| Label | Meaning |
|-------|---------|
| `area: providers` | Provider integrations & translation. |
| `area: streaming` | SSE streaming / chat path. |
| `area: tokenizer` | Token counting & estimation (ADR 003). |
| `area: observability` | Metrics, `usage_log`, tracing. |
| `area: config` | Config, hot reload, ops surface. |
| `area: resilience` | Retries, timeouts, circuit breaker, health. |
| `area: testing` | Tests, fuzzing, benchmarks. |
| `area: vision` | Image input to chat. |

### `scope:` — which capability

The gateway's three first-class capabilities. Apply one or more; omit entirely
for cross-cutting work (config, resilience, tokenizer, testing, observability)
that isn't tied to a single capability.

| Label | Meaning |
|-------|---------|
| `scope: chat` | Chat/LLM completions capability. |
| `scope: embedding` | Embeddings capability. |
| `scope: reranking` | Reranking capability. |

## Security

Please do not open public issues for vulnerabilities — see [`SECURITY.md`](SECURITY.md)
for responsible disclosure.
