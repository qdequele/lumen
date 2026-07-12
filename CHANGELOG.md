# Changelog

All notable changes to Ferrogate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
