# LUMEN

> **L**ightweight **U**nified **M**odel **EN**dpoint

A universal, self-hostable LLM gateway written in Rust. One OpenAI-compatible
endpoint in front of many providers - for **chat**, **embeddings** and
**reranking** alike. It is designed to be light, fast and sovereign: a single
static binary, **zero telemetry**, and prompts that are **never logged by
default**.

LUMEN is an alternative to LiteLLM (Python, heavier) and OpenRouter (SaaS,
not self-hostable). The gateway's own overhead is **microseconds, off-network**:
the per-request CPU work it adds is **~3.2 µs median** (resilience executor +
OpenAI-surface (de)serialization), measured with `cargo bench` on Apple Silicon.
Streaming forwards upstream bytes verbatim with no per-chunk re-serialization
(see [ADR 004](docs/adr/004-streaming-passthrough.md)). A full, reproducible
head-to-head against LiteLLM under load ships in [`bench/`](bench/README.md);
the honest numbers and the method are in
[`docs/perf-baseline.md`](docs/perf-baseline.md).

**Full documentation: <https://qdequele.github.io/lumen/>** - guides per
capability, operations (analytics, budgets, resilience), examples, and
reference.

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
| `GET  /v1/models/{id}`         | Retrieves one model (same object as the list entry); unknown id is a 404 (`LM-2001`). |
| `GET  /health`                 | Liveness. No I/O, never touches the DB or providers.    |
| `GET  /health/providers`       | Background provider-probe results (opt-in, see below).  |
| `GET  /metrics`                | Prometheus exposition.                                  |
| `POST/GET/PATCH /admin/*`      | Key/budget admin. Only mounted when auth is enabled.    |

A single model id is owned entirely by you and may serve one to three
capabilities. The router resolves each request by `(capability, model)`.

**Vision (image input):** `POST /v1/chat/completions` also accepts OpenAI's
content-parts message shape (text + `image_url` parts) for any model whose
config opts in with `modalities = ["text", "image"]` (default `["text"]`).
OpenAI-family kinds and `vllm` forward image parts verbatim; `anthropic` and
`google` translate them. See [`docs/providers.md`](docs/providers.md#vision-image-input).

## 5-minute quickstart

Zero to a successful **chat + embed + rerank** request. For ready-made
scenario configs (RAG, multi-tenant, fallback chains, …), see
[`examples/`](examples/).

### 1. Write a minimal `config.toml`

The ids below (`gpt-4o`, `text-embedding-3-small`, `rerank-english`) are the
same ones used throughout [`config.example.toml`](config.example.toml). This
minimal file needs an OpenAI key (chat + embeddings) and a Cohere key (rerank).

```toml
# config.toml - minimal quickstart config
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

**Docker** (the released image; sets `LUMEN_SERVER__HOST=0.0.0.0` for you):

```bash
docker run -p 8080:8080 \
  -v ./config.toml:/config.toml \
  -e OPENAI_API_KEY=sk-... \
  -e COHERE_API_KEY=... \
  ghcr.io/qdequele/lumen:latest
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

Stream it by adding `"stream": true` - the response becomes `text/event-stream`
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
non-empty (an empty list is rejected with `LM-2010`).

## Providers × capabilities

Twenty provider kinds: nine **native** integrations plus eleven
**OpenAI-compatible** hosts. The `kind` string is what you put in a
`[[providers]]` block. **Self-hosted** kinds are keyless and require a
`base_url`; hosted kinds read their API key from the env var named by
`api_key_env`.

| `kind`      | Chat | Embed | Rerank | Auth                  | Notes                          |
|-------------|:----:|:-----:|:------:|-----------------------|--------------------------------|
| `openai`    |  ✅  |  ✅   |        | `api_key_env`         |                                |
| `mistral`   |  ✅  |  ✅   |        | `api_key_env`         | OpenAI-compatible              |
| `anthropic` |  ✅  |       |        | `api_key_env`         | bidirectional translation      |
| `google`    |  ✅  |       |        | `api_key_env`         | Gemini                         |
| `cohere`    |      |  ✅   |   ✅   | `api_key_env`         | one model can do embed+rerank  |
| `jina`      |      |  ✅   |   ✅   | `api_key_env`         |                                |
| `voyage`    |      |  ✅   |   ✅   | `api_key_env`         |                                |
| `mixedbread`|      |      |   ✅   | `api_key_env`         | `mxbai-rerank-*`               |
| `pinecone`  |      |      |   ✅   | `api_key_env`         | `Api-Key` header; reports units |
| `nvidia`    |      |      |   ✅   | keyless, **`base_url`** | NIM `/v1/ranking`; logit scores |
| `tei`       |      |  ✅   |   ✅   | keyless, **`base_url`** | self-hosted (Text Embeddings Inference) |
| `ollama`    |  ✅  |  ✅   |        | keyless, **`base_url`** | self-hosted; chat via its OpenAI-compatible `/v1` |

**OpenAI-compatible hosts** (chat + embed, reusing the OpenAI path with a
built-in base URL): `groq`, `together`, `fireworks`, `deepseek`, `openrouter`,
`perplexity`, `xai`, `deepinfra`, `huggingface` (HF Inference router),
`cloudflare` (Workers AI - `base_url` carries your account id), and `vllm` (any
self-hosted OpenAI-compatible server: vLLM, llama.cpp, LM Studio, …). Anything
else that speaks the OpenAI format works via `kind = "openai"` + a `base_url`.
Note that `groq`, `deepseek`, `openrouter`, `perplexity` and `xai` serve chat
only (no upstream embeddings API): declaring `embed` on them is rejected at
config load, unless a custom `base_url` fronts the host with an
embedding-capable proxy. See the capability table in
[`docs/providers.md`](docs/providers.md).
`cloudflare` additionally serves **rerank** (BAAI `bge-reranker-*`) through
Workers AI's native `/ai/run/{model}` endpoint rather than the OpenAI path -
see [`docs/providers.md`](docs/providers.md). The `together` kind additionally
serves **rerank** (LlamaRank) natively.

Per-provider setup (env var, `base_url`, defaults, batch limits) is in
[`docs/providers.md`](docs/providers.md).

## Features

Each area is summarized here; the linked docs and ADRs carry the detail.

### Auth, keys & hard budgets

Off by default - with `[auth].enabled = false` the gateway is an open proxy with
no database at all. When enabled (requires `LUMEN_MASTER_KEY`, 64 hex
chars), it adds **virtual keys**, **hard budgets** and **RPM/TPM quotas**, all
enforced **in memory before any upstream call**, so a rejected request never
spends. See [Keys, quotas & budgets](https://qdequele.github.io/lumen/operations/keys-budgets.html).

### Resilience

Survives flaky upstreams without becoming flaky itself: **retries** with
exponential backoff + jitter (retryable failures only, never a client 4xx),
per-model **fallback chains**, a per-`(provider, model)` **circuit breaker**, and
per-phase timeouts. Optional **background health checks** publish
`GET /health/providers`. See [Resilience tuning](https://qdequele.github.io/lumen/operations/resilience.html).

### Observability & token accounting (ADR 003)

**Every** request of every capability produces a token count - upstream usage
when reported, otherwise a local byte-heuristic estimate flagged
`"estimated": true`. Never a silent zero. Surfaced three ways: in the response
body, on `/metrics`, and (when auth is on) in the `usage_log` table. See
[Token accounting & cost](https://qdequele.github.io/lumen/operations/token-accounting.html)
and [Metrics & dashboards](https://qdequele.github.io/lumen/operations/metrics.html).

### Config hot reload

`SIGHUP`, a file-watch, or an admin provider-key rotation triggers a reload: the
new config is validated, then the provider registry, price table, resilience
policy and the runtime-safe `[auth]` knobs are atomically swapped; in-flight
requests are unaffected. An invalid config is **rejected** - the old config
keeps serving. See
[Deployment](https://qdequele.github.io/lumen/operations/deployment.html).

### `--check-config`

`lumen --check-config [--config <PATH>]` validates a config file the same way
the server does at boot (parsing, semantic validation and provider registry
construction) and exits: `0` if valid, non-zero otherwise. It binds no
listener, opens no database, and contacts no provider, so it is safe to run
in a CI or deploy pipeline ahead of a real boot. See
[Installation](https://qdequele.github.io/lumen/getting-started/installation.html).

### Security headers

Every response carries a standard set of security headers. TLS is
intentionally left to a terminating reverse proxy. See
[Deployment](https://qdequele.github.io/lumen/operations/deployment.html).

## Configuration

Everything is one TOML file (plus `LUMEN_*` env overrides, using `__` for
nesting, e.g. `LUMEN_SERVER__PORT=9090`). The exhaustively commented reference
is [`config.example.toml`](config.example.toml); the full walkthrough lives in
the book's
[Configuration basics](https://qdequele.github.io/lumen/getting-started/configuration.html).

API keys are **never** written in the config - a provider references the *name*
of the env var that holds its key.

## Benchmarks

From [`docs/perf-baseline.md`](docs/perf-baseline.md) (Apple Silicon, release
profile, `rustc 1.97.0`):

| Measure                                             | Value             |
|-----------------------------------------------------|-------------------|
| Executor around an instant provider (`executor_overhead_chat`) | ~1.21 µs median |
| Parse a chat request (`json_request_deserialize`)   | ~1.34 µs median   |
| Serialize a chat response (`json_response_serialize`)| ~0.60 µs median  |
| **Total added CPU per non-streaming chat request**  | **~3.2 µs median**|
| Idle RSS (release binary, one provider)             | ~8.8 MB           |

Reproduce the in-process numbers:

```bash
cargo bench -p server --bench gateway_overhead
```

The full loaded head-to-head against LiteLLM (added latency p50/p99, RAM, req/s)
is a one-command Docker + k6 harness - see [`bench/README.md`](bench/README.md).
That comparison is not executed in the recording environment; the microsecond
off-network overhead above is what is asserted, and the harness lets anyone
produce the loaded numbers on their own hardware.

## Security

LUMEN runs inside your own trust boundary. Provider keys are referenced by
env-var name (or encrypted at rest under `LUMEN_MASTER_KEY`), never logged,
never returned in errors. Prompts and responses are never logged by default.
Vulnerability reporting and the full security model are in
[`SECURITY.md`](SECURITY.md).

## Reference

- Error codes: [`docs/errors.md`](docs/errors.md)
- Provider setup: [`docs/providers.md`](docs/providers.md)
- Performance: [`docs/perf-baseline.md`](docs/perf-baseline.md)
- Architecture decisions: [`docs/adr/`](docs/adr/)
- Changelog: [`CHANGELOG.md`](CHANGELOG.md)
- Examples: [examples/](examples/)
- Documentation site: <https://qdequele.github.io/lumen/>

## License

Apache-2.0.
