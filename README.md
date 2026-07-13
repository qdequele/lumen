# Ferrogate

A universal, self-hostable LLM gateway written in Rust. One OpenAI-compatible
endpoint in front of many providers — for **chat**, **embeddings** and
**reranking** alike. It is designed to be light, fast and sovereign: a single
static binary, **zero telemetry**, and prompts that are **never logged by
default**.

Ferrogate is an alternative to LiteLLM (Python, heavier) and OpenRouter (SaaS,
not self-hostable). The gateway's own overhead is **microseconds, off-network**:
the per-request CPU work it adds is **~3.2 µs median** (M6 resilience executor +
OpenAI-surface (de)serialization), measured with `cargo bench` on Apple Silicon.
Streaming forwards upstream bytes verbatim with no per-chunk re-serialization
(see [ADR 004](docs/adr/004-streaming-passthrough.md)). A full, reproducible
head-to-head against LiteLLM under load ships in [`bench/`](bench/README.md);
the honest numbers and the method are in
[`docs/perf-baseline.md`](docs/perf-baseline.md).

## Contents

- [Capabilities & API](#capabilities--api)
- [5-minute quickstart](#5-minute-quickstart)
- [Providers × capabilities](#providers--capabilities)
- [Features](#features)
- [Configuration](#configuration)
- [Benchmarks](#benchmarks)
- [Security](#security)

## Capabilities & API

| Method & path                 | What it does                                            |
|--------------------------------|---------------------------------------------------------|
| `POST /v1/chat/completions`    | Chat completions, OpenAI format, streaming SSE.         |
| `POST /v1/embeddings`          | Embeddings, OpenAI format.                              |
| `POST /v1/rerank`              | Reranking, Cohere format (`query`, `documents`, `top_n`).|
| `GET  /v1/models`              | Lists configured models with a `capabilities` array.    |
| `GET  /health`                 | Liveness. No I/O, never touches the DB or providers.    |
| `GET  /health/providers`       | Background provider-probe results (opt-in, see below).  |
| `GET  /metrics`                | Prometheus exposition.                                  |
| `POST/GET/PATCH /admin/*`      | Key/budget admin. Only mounted when auth is enabled.    |

A single model id is owned entirely by you and may serve one to three
capabilities. The router resolves each request by `(capability, model)`.

## 5-minute quickstart

Zero to a successful **chat + embed + rerank** request.

### 1. Write a minimal `config.toml`

The ids below (`gpt-4o`, `text-embedding-3-small`, `rerank-english`) are the
same ones used throughout [`config.example.toml`](config.example.toml). This
minimal file needs an OpenAI key (chat + embeddings) and a Cohere key (rerank).

```toml
# config.toml — minimal quickstart config
[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
upstream_id = "gpt-4o-2024-08-06"
capabilities = ["chat"]

[[providers.models]]
id = "text-embedding-3-small"
capabilities = ["embed"]

[[providers]]
name = "cohere"
kind = "cohere"
api_key_env = "COHERE_API_KEY"

[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
```

### 2. Run it

**Docker** (the released image; sets `FERROGATE_SERVER__HOST=0.0.0.0` for you):

```bash
docker run -p 8080:8080 \
  -v ./config.toml:/config.toml \
  -e OPENAI_API_KEY=sk-... \
  -e COHERE_API_KEY=... \
  ghcr.io/meilisearch/ferrogate:latest
```

**From source** (needs a recent stable Rust toolchain):

```bash
export OPENAI_API_KEY=sk-...
export COHERE_API_KEY=...
cargo run -p server -- --config config.toml
```

> The bundled [`config.example.toml`](config.example.toml) also runs as-is
> (`cargo run -p server -- --config config.example.toml`); it additionally wires
> Anthropic/Ollama/Jina/Voyage/TEI and demonstrates fallbacks. Providers whose
> API-key env var is unset are only rejected when a request actually routes to
> them, so a partial set of keys is fine.

By default auth is **off**, so these requests need no `Authorization` header.

### 3. Chat completion

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Say hello in one word."}]
  }'
```

Stream it by adding `"stream": true` — the response becomes `text/event-stream`
with `data: {…}` frames and a terminal `data: [DONE]`.

### 4. Embeddings

```bash
curl -s http://localhost:8080/v1/embeddings \
  -H 'content-type: application/json' \
  -d '{
    "model": "text-embedding-3-small",
    "input": ["the quick brown fox", "a lazy dog"]
  }'
```

### 5. Rerank

```bash
curl -s http://localhost:8080/v1/rerank \
  -H 'content-type: application/json' \
  -d '{
    "model": "rerank-english",
    "query": "What is the capital of France?",
    "documents": ["Paris is the capital of France.", "Berlin is in Germany."],
    "top_n": 2
  }'
```

Results come back sorted by descending `relevance_score`. `documents` must be
non-empty (an empty list is rejected with `FG-2010`).

## Providers × capabilities

Nine provider kinds. The `kind` string is what you put in a `[[providers]]`
block. **Self-hosted** kinds are keyless and require a `base_url`; hosted kinds
read their API key from the env var named by `api_key_env`.

| `kind`      | Chat | Embed | Rerank | Auth                  | Notes                          |
|-------------|:----:|:-----:|:------:|-----------------------|--------------------------------|
| `openai`    |  ✅  |  ✅   |        | `api_key_env`         |                                |
| `mistral`   |  ✅  |  ✅   |        | `api_key_env`         | OpenAI-compatible              |
| `anthropic` |  ✅  |       |        | `api_key_env`         | bidirectional translation      |
| `google`    |  ✅  |       |        | `api_key_env`         | Gemini                         |
| `cohere`    |      |  ✅   |   ✅   | `api_key_env`         | one model can do embed+rerank  |
| `jina`      |      |  ✅   |   ✅   | `api_key_env`         |                                |
| `voyage`    |      |  ✅   |   ✅   | `api_key_env`         |                                |
| `tei`       |      |  ✅   |   ✅   | keyless, **`base_url`** | self-hosted (Text Embeddings Inference) |
| `ollama`    |      |  ✅   |        | keyless, **`base_url`** | self-hosted                    |

Per-provider setup (env var, `base_url`, batch limits) is in
[`docs/providers.md`](docs/providers.md).

## Features

Each area is summarized here; the linked docs and ADRs carry the detail.

### Auth, keys & hard budgets (M5)

Off by default — with `[auth].enabled = false` the gateway is an open proxy with
no database at all. When enabled (requires `FERROGATE_MASTER_KEY`, 64 hex
chars), it adds **virtual keys**, **hard budgets** and **RPM/TPM quotas**, all
enforced **in memory before any upstream call**, so a rejected request never
spends. The DB is never on the request path. Refusals are `402 FG-4001`
(budget), `429 FG-4002` (RPM), `429 FG-4003` (TPM); a missing/invalid key is
`401 FG-4004`. Keys and budgets are managed via the `/admin/*` API, gated by the
master key. See [`SECURITY.md`](SECURITY.md).

### Resilience (M6)

Survives flaky upstreams without becoming flaky itself: **retries** with
exponential backoff + jitter (retryable failures only, never a client 4xx),
per-model **fallback chains**, a per-`(provider, model)` **circuit breaker**, and
three distinct **per-phase timeouts** (`FG-3012` connect, `FG-3011` first-token,
`FG-3013` total). Optional **background health checks** publish
`GET /health/providers`. The model that actually served a request is reported in
the `x-ferrogate-model-used` response header. All configured under
`[resilience]`; design in [ADR 005](docs/adr/005-resilience-execution.md).

### Observability & token accounting (M5, ADR 003)

**Every** request of every capability produces a token count — upstream usage
when reported, otherwise a local byte-heuristic estimate flagged
`"estimated": true`. Never a silent zero. Surfaced three ways: in the response
body, on `/metrics`, and (when auth is on) in the `usage_log` table. Key
metrics: `ferrogate_tokens_total{capability,model,provider,direction,estimated}`,
`ferrogate_rerank_search_units_total`, `ferrogate_circuit_state{provider,model}`,
`ferrogate_provider_up{provider}`, `ferrogate_usage_log_dropped_total`,
`ferrogate_config_reloads_total` / `ferrogate_config_reload_failures_total`. The
usage log is written on a bounded async channel that **drops rather than blocks**
the request path, and stores token counts, cost and metadata labels — **never
message content**. See [ADR 003](docs/adr/003-token-accounting.md) and
[ADR 002](docs/adr/002-request-metadata-header.md) for the `x-ferrogate-metadata`
header.

### Config hot reload (M7)

`SIGHUP` or a file-watch triggers a reload: the new config is validated, then the
provider registry is atomically swapped. In-flight requests are unaffected. An
invalid config is **rejected** — the old config keeps serving and
`ferrogate_config_reload_failures_total` increments.

### Security headers (M7)

Every response carries `X-Content-Type-Options: nosniff`,
`X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, and
`Content-Security-Policy: default-src 'none'`. TLS is intentionally left to a
terminating reverse proxy — see [`SECURITY.md`](SECURITY.md).

## Configuration

Everything is one TOML file (plus `FERROGATE_*` env overrides, using `__` for
nesting, e.g. `FERROGATE_SERVER__PORT=9090`). The exhaustively commented
reference is [`config.example.toml`](config.example.toml), with sections:

- `log_format` — `"pretty"` (default) or `"json"`.
- `[server]` — bind host/port, body limit, first-token timeout, SSE heartbeat.
- `[auth]` — virtual keys, budgets, quotas, usage log (off by default).
- `[telemetry]` — which `x-ferrogate-metadata` keys become Prometheus labels.
- `[resilience]` — retries, circuit breaker, timeouts, health checks.
- `[[providers]]` / `[[providers.models]]` — upstreams, model ids, capabilities,
  prices, and per-model `fallbacks`.

API keys are **never** written in the config — a provider references the *name*
of the env var that holds its key.

## Benchmarks

From [`docs/perf-baseline.md`](docs/perf-baseline.md) (Apple Silicon, release
profile, `rustc 1.97.0`):

| Measure                                             | Value             |
|-----------------------------------------------------|-------------------|
| M6 executor around an instant provider (`executor_overhead_chat`) | ~1.21 µs median |
| Parse a chat request (`json_request_deserialize`)   | ~1.34 µs median   |
| Serialize a chat response (`json_response_serialize`)| ~0.60 µs median  |
| **Total added CPU per non-streaming chat request**  | **~3.2 µs median**|
| Idle RSS (release binary, one provider)             | ~8.8 MB           |

Reproduce the in-process numbers:

```bash
cargo bench -p server --bench gateway_overhead
```

The full loaded head-to-head against LiteLLM (added latency p50/p99, RAM, req/s)
is a one-command Docker + k6 harness — see [`bench/README.md`](bench/README.md).
That comparison is not executed in the recording environment; the microsecond
off-network overhead above is what is asserted, and the harness lets anyone
produce the loaded numbers on their own hardware.

## Security

Ferrogate runs inside your own trust boundary. Provider keys are referenced by
env-var name (or encrypted at rest under `FERROGATE_MASTER_KEY`), never logged,
never returned in errors. Prompts and responses are never logged by default.
Vulnerability reporting and the full security model are in
[`SECURITY.md`](SECURITY.md).

## Reference

- Error codes: [`docs/errors.md`](docs/errors.md)
- Provider setup: [`docs/providers.md`](docs/providers.md)
- Performance: [`docs/perf-baseline.md`](docs/perf-baseline.md)
- Architecture decisions: [`docs/adr/`](docs/adr/)
- Changelog: [`CHANGELOG.md`](CHANGELOG.md)

## License

Apache-2.0.
