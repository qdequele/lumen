# Examples

Runnable scenario configs for LUMEN. Each directory is self-contained:
`config.toml` (the gateway config), `README.md` (what it demonstrates and
any prerequisites), and `run.sh` (the requests to fire once the gateway is
up).

- **minimal-chat**: the smallest possible config, one provider and one
  model, non-streaming and streaming chat against OpenAI.
- **self-hosted**: fully keyless setup, chat and embeddings via Ollama plus
  reranking via TEI, works offline.
- **multi-provider-fallback**: cross-vendor chat fallback, OpenAI primary
  with an Anthropic fallback model, and how to watch the circuit breaker
  trip and the fallback fire.
- **rag-pipeline**: the two calls behind a RAG pipeline, embeddings via
  OpenAI at index time and reranking via Cohere at query time.
- **multi-tenant-analytics**: per-tenant cost attribution, virtual keys
  with a hard budget plus `x-lumen-metadata` turned into Prometheus labels.

More scenarios live alongside these; see each directory's own `README.md`
for what it adds.

## Running a scenario

Every scenario follows the same two-terminal recipe:

```bash
# terminal 1 - start the gateway with the scenario's config
cargo run -p server -- --config examples/minimal-chat/config.toml
# terminal 2 - fire the scenario's requests
./examples/minimal-chat/run.sh
```

Swap `minimal-chat` for any other scenario directory name.

Every config in this directory passes `lumen --check-config` in CI. Provider
keys are never written into a config file; each config reads them from the
environment variable named by its `api_key_env` field (see each scenario's
`README.md` for which variables it needs).
