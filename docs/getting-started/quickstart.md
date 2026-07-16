# Quickstart

Zero to a successful **chat + embed + rerank** request.

## 1. Minimal config

The ids below (`gpt-4o`, `text-embedding-3-small`, `rerank-english`) are the
same ones used throughout
[`config.example.toml`](https://github.com/qdequele/lumen/blob/main/config.example.toml).
This minimal file needs an OpenAI key (chat + embeddings) and a Cohere key
(rerank).

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

## 2. Run

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

By default auth is **off**, so these requests need no `Authorization` header.
Providers whose API-key env var is unset are only rejected when a request
actually routes to them, so a partial set of keys is fine.

If you turn auth on (`[auth] enabled = true`), bootstrap your first virtual
key with `lumen keys create --name bootstrap` - it runs offline against the
auth database, before the server is ever started. See
[Keys, quotas & budgets](../operations/keys-budgets.md#bootstrapping-the-first-key-cli).

## 3. Chat

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Say hello in one word."}]
  }'
```

The response is the OpenAI chat completion envelope, including a `usage`
object with the token count.

## 4. Embeddings

```bash
curl -s http://localhost:8080/v1/embeddings \
  -H 'content-type: application/json' \
  -d '{
    "model": "text-embedding-3-small",
    "input": ["the quick brown fox", "a lazy dog"]
  }'
```

The response carries `data[].embedding` for each input, plus a `usage`
object.

## 5. Rerank

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
non-empty: an empty list is rejected with `LM-2010`.

## Next steps

- [Chat completions](../chat/completions.md)
- [Embeddings](../embeddings/embeddings.md)
- [Reranking](../reranking/reranking.md)
- [Token accounting](../operations/token-accounting.md)
