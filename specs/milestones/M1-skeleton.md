# M1 — Skeleton & foundations

## Objective
A binary that starts up, serves /health and /metrics, loads its config, with the full crate structure and CI in place. No providers yet.

## Tasks

### 1.1 Workspace
- [x] `Cargo.toml` workspace with crates: `core`, `providers`, `router`, `auth`, `telemetry`, `server`
- [x] Release profile: `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`
- [x] `rust-toolchain.toml` (stable), `.gitignore`, Apache-2.0 license

### 1.2 crates/core
- [x] Request/response types: `ChatRequest/Response/Chunk`, `EmbedRequest/Response`, `RerankRequest/Response` (serde, fields from the OpenAI format + Cohere rerank)
- [x] `ChatProvider`, `EmbeddingProvider`, `RerankProvider` traits (signatures from CLAUDE.md, with `CancellationToken`)
- [x] `ProviderError` (thiserror): variants `Upstream { provider, status, retryable }`, `Timeout`, `Cancelled`, `Translation`, `RateLimited { retry_after }`
- [x] `GatewayError` with stable code `LM-XXXX`, conversion to a JSON response: `{"error": {"code": "LM-1001", "message": "...", "type": "invalid_request|upstream_error|internal"}}`
- [x] `Capability` enum: `Chat | Embed | Rerank`

### 1.3 crates/server
- [x] axum binary: `GET /health` (always responds 200 if the process is alive — no I/O), `GET /metrics` (empty Prometheus registry for now)
- [x] Graceful shutdown on SIGTERM/SIGINT: stop accepting, drain in-flight requests (30 s timeout)
- [x] tower middleware: request-id, tracing span per request, body size limit (configurable, default 10 MB)

### 1.4 Config
- [x] figment: `config.toml` + override via `LUMEN_*` env vars
- [x] Structs: `ServerConfig { host, port, body_limit }`, `ProviderConfig { name, kind, api_key_env, base_url, models: Vec<ModelConfig> }`, `ModelConfig { id, upstream_id, capabilities }`
- [x] API keys are referenced by env var NAME (`api_key_env = "OPENAI_API_KEY"`), never in cleartext in the TOML
- [x] Boot-time validation: invalid config = exit(1) with a precise message (file, field, reason)
- [x] Commented `config.example.toml`

### 1.5 CI
- [x] `.github/workflows/ci.yml`: fmt --check, clippy -D warnings, test, on push + PR

## Acceptance criteria
1. `cargo run -p server -- --config config.example.toml` starts in < 100 ms and logs the list of loaded models (without the keys).
2. `curl :8080/health` → 200 `{"status":"ok"}` even if no key env var is set.
3. Config with an unknown field or an invalid port → exit(1), error message naming the field.
4. A test captures the boot logs and verifies that no API key value appears in them.
5. SIGTERM during an in-flight request (test with a slow test route) → the request completes, then the process exits with code 0.
6. CI green.

## Pitfalls to avoid
- Do not put a DB connection in /health (LiteLLM lesson #15526: readiness probes that fail under load → cascade of restarts).
- Do not naively derive `Debug` on config structs containing secret references.
