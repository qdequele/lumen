# Providers

LUMEN ships nine built-in provider kinds. Each `[[providers]]` block in your
config selects one with a `kind` string and gives it a unique `name` (your own
label). Each `[[providers.models]]` block under it exposes a model to clients:

```toml
[[providers]]
name = "my-openai"        # your label; must be unique
kind = "openai"           # selects the built-in implementation
api_key_env = "OPENAI_API_KEY"   # NAME of the env var holding the key
# base_url = "https://…"  # optional override (required for self-hosted kinds)

[[providers.models]]
id = "gpt-4o"             # the id clients send (owned entirely by you)
upstream_id = "gpt-4o-2024-08-06"   # what LUMEN sends upstream (defaults to `id`)
capabilities = ["chat"]   # any of "chat", "embed", "rerank"
```

Rules that apply to every provider:

- **API keys are never in the config.** `api_key_env` names an environment
  variable; LUMEN reads it only when a request actually routes to that
  provider. A hosted provider whose env var is unset fails only at use, not at
  boot — a partial set of keys is fine.
- **Model ids are globally unique** across all providers. A collision aborts
  startup and names both offending providers. Several ids may map to one
  `upstream_id` (versioned aliasing).
- **`capabilities` must match the kind.** A model can only declare capabilities
  its provider kind implements (table below); this is validated at boot.
- **`base_url`** is an optional override for hosted kinds, and **required** for
  the self-hosted kinds (`tei`, `ollama`), which are keyless.
- **Batching**: an embed request with more inputs than the provider's batch
  limit is split into sub-batches, run with bounded concurrency, and reassembled
  in the original order. The limits below are built in.

| `kind`      | Chat | Embed | Rerank | `api_key_env` | `base_url`     | Embed batch limit |
|-------------|:----:|:-----:|:------:|:-------------:|:--------------:|:-----------------:|
| `openai`    |  ✅  |  ✅   |        | required      | optional       | 2048              |
| `mistral`   |  ✅  |  ✅   |        | required      | optional       | 512               |
| `anthropic` |  ✅  |       |        | required      | optional       | —                 |
| `google`    |  ✅  |       |        | required      | optional       | —                 |
| `cohere`    |      |  ✅   |   ✅   | required      | optional       | 96                |
| `jina`      |      |  ✅   |   ✅   | required      | optional       | 2048              |
| `voyage`    |      |  ✅   |   ✅   | required      | optional       | 128               |
| `tei`       |      |  ✅   |   ✅   | keyless       | **required**   | 32                |
| `ollama`    |      |  ✅   |        | keyless       | **required**   | 512               |

---

## openai

- **kind**: `openai` · **capabilities**: chat, embed
- **Auth**: `api_key_env` (e.g. `OPENAI_API_KEY`), sent as a bearer token.
- **base_url**: optional; defaults to OpenAI's public API. Set it to point at any
  OpenAI-compatible endpoint.
- **Embed batch limit**: 2048 inputs per upstream call.

```toml
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
```

## mistral

- **kind**: `mistral` · **capabilities**: chat, embed (OpenAI-compatible).
- **Auth**: `api_key_env` (e.g. `MISTRAL_API_KEY`), bearer token.
- **base_url**: optional override.
- **Embed batch limit**: 512.

```toml
[[providers]]
name = "mistral"
kind = "mistral"
api_key_env = "MISTRAL_API_KEY"

[[providers.models]]
id = "mistral-small"
upstream_id = "mistral-small-latest"
capabilities = ["chat"]
```

## anthropic

- **kind**: `anthropic` · **capabilities**: chat only.
- **Auth**: `api_key_env` (e.g. `ANTHROPIC_API_KEY`). LUMEN authenticates
  with the `x-api-key` / `anthropic-version` headers, not a bearer token.
- **Translation**: OpenAI ⇄ Anthropic is bidirectional, including tools and
  streaming events, so clients keep using the OpenAI wire format.
- **base_url**: optional override.

```toml
[[providers]]
name = "anthropic"
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[[providers.models]]
id = "claude-3-5-sonnet"
upstream_id = "claude-3-5-sonnet-20241022"
capabilities = ["chat"]
```

## google

- **kind**: `google` · **capabilities**: chat only (Gemini).
- **Auth**: `api_key_env` (e.g. `GEMINI_API_KEY`). The key rides the
  `x-goog-api-key` header, never the URL.
- **Translation**: OpenAI ⇄ Gemini, including streaming (`streamGenerateContent`).
- **base_url**: optional override.

```toml
[[providers]]
name = "google"
kind = "google"
api_key_env = "GEMINI_API_KEY"

[[providers.models]]
id = "gemini-2.0-flash"
upstream_id = "gemini-2.0-flash"
capabilities = ["chat"]
```

## cohere

- **kind**: `cohere` · **capabilities**: embed, rerank. A single model can serve
  both.
- **Auth**: `api_key_env` (e.g. `COHERE_API_KEY`), bearer token.
- **Embed batch limit**: 96.
- **Cost**: rerank is billed in search units (`cost_per_1k_searches`).

```toml
[[providers]]
name = "cohere"
kind = "cohere"
api_key_env = "COHERE_API_KEY"

[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
cost_per_1k_searches = 2.0

[[providers.models]]
id = "embed-multilingual"
upstream_id = "embed-v4.0"
capabilities = ["embed", "rerank"]
```

## jina

- **kind**: `jina` · **capabilities**: embed, rerank (hosted).
- **Auth**: `api_key_env` (e.g. `JINA_API_KEY`), bearer token.
- **Embed batch limit**: 2048.

```toml
[[providers]]
name = "jina"
kind = "jina"
api_key_env = "JINA_API_KEY"

[[providers.models]]
id = "jina-rerank"
upstream_id = "jina-reranker-v2-base-multilingual"
capabilities = ["rerank"]
```

## voyage

- **kind**: `voyage` · **capabilities**: embed, rerank (hosted).
- **Auth**: `api_key_env` (e.g. `VOYAGE_API_KEY`), bearer token.
- **Embed batch limit**: 128.

```toml
[[providers]]
name = "voyage"
kind = "voyage"
api_key_env = "VOYAGE_API_KEY"

[[providers.models]]
id = "voyage-rerank"
upstream_id = "rerank-2"
capabilities = ["rerank"]
```

## tei (self-hosted)

- **kind**: `tei` · **capabilities**: embed, rerank.
- **Auth**: keyless.
- **base_url**: **required** — points at your Text Embeddings Inference server.
  TEI serves one model per process, so `upstream_id` is ignored by the upstream
  but kept for your own clarity.
- **Embed batch limit**: 32.

```toml
[[providers]]
name = "tei-local"
kind = "tei"
base_url = "http://localhost:8081"

[[providers.models]]
id = "bge-reranker"
upstream_id = "BAAI/bge-reranker-large"
capabilities = ["rerank"]
```

## ollama (self-hosted)

- **kind**: `ollama` · **capabilities**: embed.
- **Auth**: keyless.
- **base_url**: **required** — points at your Ollama server.
- **Embed batch limit**: 512.
- **Tip**: a local model may take a while to load into VRAM on its first call —
  relax `first_token_timeout_ms` / `total_timeout_ms` on the provider block (see
  `config.example.toml`).

```toml
[[providers]]
name = "ollama-local"
kind = "ollama"
base_url = "http://localhost:11434"
first_token_timeout_ms = 60000
total_timeout_ms = 120000

[[providers.models]]
id = "nomic-embed"
upstream_id = "nomic-embed-text"
capabilities = ["embed"]
```

---

## Fallbacks across providers

Any model can name an ordered list of `fallbacks` — models that back it when its
provider exhausts retries or its circuit is open. Each fallback must exist and
serve every capability of the model it backs (validated at boot), which lets you
survive a single-vendor outage by spanning providers:

```toml
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["claude-3-5-sonnet"]     # different vendor, same capability
```

See [`docs/adr/005-resilience-execution.md`](adr/005-resilience-execution.md)
for the resolution and circuit-breaker details, and `config.example.toml` for a
fully worked multi-provider setup including a three-vendor rerank fallback chain.
