# Examples

Runnable scenario configs live in
[`examples/`](https://github.com/qdequele/lumen/tree/main/examples) at the
root of the repository. Each directory is self-contained: a `config.toml`
(the gateway config), a `README.md` (what it demonstrates and any
prerequisites), and a `run.sh` (the requests to fire once the gateway is
up).

Every scenario follows the same two-terminal recipe: start the gateway with
the scenario's config in one terminal, then fire its `run.sh` in another.
Provider keys are never written into a config file; each config reads them
from the environment variable named by its `api_key_env` field.

Every `config.toml` in `examples/` passes `lumen --check-config` in CI.

## minimal-chat

The smallest possible LUMEN config: one provider (OpenAI), one model
(`gpt-4o`), chat only.

**Demonstrates**: non-streaming chat via `POST /v1/chat/completions`, and
the same endpoint with `"stream": true`.

**Env vars**: `OPENAI_API_KEY`.

```bash
# terminal 1
export OPENAI_API_KEY=sk-...
cargo run -p server -- --config examples/minimal-chat/config.toml

# terminal 2
./examples/minimal-chat/run.sh
```

[examples/minimal-chat on GitHub](https://github.com/qdequele/lumen/tree/main/examples/minimal-chat)

## self-hosted

A fully keyless config: no cloud provider, no API key anywhere. Chat and
embeddings come from [Ollama](https://ollama.com), reranking from
[TEI](https://github.com/huggingface/text-embeddings-inference). Everything
runs offline once the models are pulled.

**Demonstrates**: chat against Ollama's OpenAI-compatible endpoint,
embeddings against Ollama's native endpoint, and reranking against a local
TEI server.

**Env vars**: none. Requires Ollama running locally with `llama3.2` and
`nomic-embed-text` pulled, and (optionally) TEI serving
`BAAI/bge-reranker-large` on port 8081.

```bash
# terminal 1
cargo run -p server -- --config examples/self-hosted/config.toml

# terminal 2
./examples/self-hosted/run.sh
```

[examples/self-hosted on GitHub](https://github.com/qdequele/lumen/tree/main/examples/self-hosted)

## multi-provider-fallback

Cross-vendor chat fallback: `gpt-4o` (OpenAI) is primary, with
`claude-3-5-sonnet` (Anthropic) declared as its `fallbacks`.

**Demonstrates**: the `x-lumen-model-used` response header reporting
whether the primary or the fallback served a request, and how the circuit
breaker trips after repeated failures on the primary. See
[Resilience tuning](operations/resilience.md).

**Env vars**: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` (the Anthropic key is
only used if the fallback actually fires).

```bash
# terminal 1
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p server -- --config examples/multi-provider-fallback/config.toml

# terminal 2
./examples/multi-provider-fallback/run.sh
```

[examples/multi-provider-fallback on GitHub](https://github.com/qdequele/lumen/tree/main/examples/multi-provider-fallback)

## rag-pipeline

The two calls behind a typical RAG pipeline, wired to two different
providers: embeddings via OpenAI (`text-embedding-3-small`) at index time,
reranking via Cohere (`rerank-english`) at query time.

**Demonstrates**: `POST /v1/embeddings` embedding a small document corpus,
then `POST /v1/rerank` re-scoring the same documents against a query with
`"top_n": 2`. See [Embeddings](embeddings/embeddings.md) and
[Reranking](reranking/reranking.md).

**Env vars**: `OPENAI_API_KEY`, `COHERE_API_KEY`.

```bash
# terminal 1
export OPENAI_API_KEY=sk-...
export COHERE_API_KEY=...
cargo run -p server -- --config examples/rag-pipeline/config.toml

# terminal 2
./examples/rag-pipeline/run.sh
```

[examples/rag-pipeline on GitHub](https://github.com/qdequele/lumen/tree/main/examples/rag-pipeline)

## multi-tenant-analytics

Per-tenant cost and usage attribution: `[auth]` enabled with a virtual key
per tenant carrying a hard budget, and `[telemetry].metadata_labels`
turning the `x-lumen-metadata` header into Prometheus labels.

**Demonstrates**: creating a virtual key through the admin API
(`POST /admin/keys`, master-key bearer), tagging requests with
`x-lumen-metadata`, and slicing `lumen_tokens_total` on `/metrics` by
`org_id`. See [Usage log & multi-tenant metadata](operations/usage-log.md)
and [Keys, quotas & budgets](operations/keys-budgets.md).

**Env vars**: `OPENAI_API_KEY`, `LUMEN_MASTER_KEY` (64 hex characters,
e.g. `openssl rand -hex 32`).

```bash
# terminal 1
export OPENAI_API_KEY=sk-...
export LUMEN_MASTER_KEY=$(openssl rand -hex 32)
cargo run -p server -- --config examples/multi-tenant-analytics/config.toml

# terminal 2
export LUMEN_MASTER_KEY=...   # same value as terminal 1
./examples/multi-tenant-analytics/run.sh
```

`--check-config` on this scenario does not need `LUMEN_MASTER_KEY` set: it
only validates `config.toml`, the master key is read separately at actual
server startup. Do not export `LUMEN_MASTER_KEY` while running
`--check-config` on any scenario: the config loader merges every
`LUMEN_`-prefixed environment variable into the config, and `master_key` is
not a recognized field, so a set `LUMEN_MASTER_KEY` makes `--check-config`
fail on every scenario, not just this one.

[examples/multi-tenant-analytics on GitHub](https://github.com/qdequele/lumen/tree/main/examples/multi-tenant-analytics)
