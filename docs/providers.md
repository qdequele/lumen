# Providers

LUMEN ships twenty built-in provider kinds - nine native integrations (their own
request/response translation) plus eleven **OpenAI-compatible** hosts that reuse
the OpenAI path with a per-kind base URL. Each `[[providers]]` block in your
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
  boot - a partial set of keys is fine.
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
- **Multimodal embeddings (M9)**: declare `modalities = ["text", "image"]` on a
  model to accept image content parts on `/v1/embeddings`. `input` items may be
  strings or arrays of parts (`{"type":"text",...}` / `{"type":"image_url",...}`).
  Images are passed as `data:` URIs, or - with `[image_fetch]` enabled - as
  remote `http(s)` URLs the gateway fetches under SSRF/resource guards and
  inlines. Image input to a model without `"image"` is rejected with `LM-2003`;
  a remote URL with fetching disabled is `LM-2005`. Cohere (embed-v4) and Voyage
  embed a combined text+image vector per item; **Jina** embeds one modality per
  item (a mixed item is sent as its image, its caption text is not combined).
  See the multimodal-embeddings design spec for the full guard list.

| `kind`      | Chat | Embed | Rerank | `api_key_env` | `base_url`     | Embed batch limit |
|-------------|:----:|:-----:|:------:|:-------------:|:--------------:|:-----------------:|
| `openai`    |  ✅  |  ✅   |        | required      | optional       | 2048              |
| `mistral`   |  ✅  |  ✅   |        | required      | optional       | 512               |
| `anthropic` |  ✅  |       |        | required      | optional       | -                 |
| `google`    |  ✅  |       |        | required      | optional       | -                 |
| `cohere`    |      |  ✅   |   ✅   | required      | optional       | 96                |
| `jina`      |      |  ✅   |   ✅   | required      | optional       | 2048              |
| `voyage`    |      |  ✅   |   ✅   | required      | optional       | 128               |
| `tei`       |      |  ✅   |   ✅   | keyless       | **required**   | 32                |
| `ollama`    |      |  ✅   |        | keyless       | **required**   | 512               |

OpenAI-compatible hosts (chat + embed via the OpenAI path; a host that only
serves chat simply has no embed models configured):

| `kind`        | Chat | Embed | `api_key_env` | `base_url`   | Default base URL                          |
|---------------|:----:|:-----:|:-------------:|:------------:|-------------------------------------------|
| `groq`        |  ✅  |  ✅   | required      | optional     | `https://api.groq.com/openai/v1`          |
| `together`    |  ✅  |  ✅   | required      | optional     | `https://api.together.xyz/v1`             |
| `fireworks`   |  ✅  |  ✅   | required      | optional     | `https://api.fireworks.ai/inference/v1`   |
| `deepseek`    |  ✅  |  ✅   | required      | optional     | `https://api.deepseek.com/v1`             |
| `openrouter`  |  ✅  |  ✅   | required      | optional     | `https://openrouter.ai/api/v1`            |
| `perplexity`  |  ✅  |  ✅   | required      | optional     | `https://api.perplexity.ai`               |
| `xai`         |  ✅  |  ✅   | required      | optional     | `https://api.x.ai/v1`                     |
| `deepinfra`   |  ✅  |  ✅   | required      | optional     | `https://api.deepinfra.com/v1/openai`     |
| `huggingface` |  ✅  |  ✅   | required      | optional     | `https://router.huggingface.co/v1`        |
| `cloudflare`  |  ✅  |  ✅   | required      | **required** | - (URL embeds your account id)            |
| `vllm`        |  ✅  |  ✅   | keyless       | **required** | - (your self-hosted server)               |

All OpenAI-compatible kinds use a 2048-input embed batch limit. Anything that
speaks the OpenAI wire format but isn't listed can still be used via
`kind = "openai"` with a `base_url` override.

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
- **`input_type` override**: Cohere's embed v2 API requires an `input_type`
  and the gateway cannot know query-vs-document intent, so it defaults to
  `search_document` (the indexing case). Set `input_type` as an extra field on
  the `/v1/embeddings` request body to override it per request, e.g.
  `{"model": "embed-multilingual", "input": "...", "input_type": "search_query"}`.
  Allowed values: `search_document`, `search_query`, `classification`,
  `clustering`. An unrecognized value is rejected with `LM-1001` before any
  upstream call. Ignored (harmlessly) by every other provider.

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
- **base_url**: **required** - points at your Text Embeddings Inference server.
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
- **base_url**: **required** - points at your Ollama server.
- **Embed batch limit**: 512.
- **Tip**: a local model may take a while to load into VRAM on its first call -
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

## OpenAI-compatible hosts

`groq`, `together`, `fireworks`, `deepseek`, `openrouter`, `perplexity`, `xai`,
`deepinfra` and `huggingface` all work the same way: set the `kind`, point
`api_key_env` at the host's token, and (optionally) override `base_url`. The
built-in default base URL is used otherwise.

```toml
[[providers]]
name = "groq"
kind = "groq"
api_key_env = "GROQ_API_KEY"
[[providers.models]]
id = "fast"
upstream_id = "llama-3.3-70b-versatile"
capabilities = ["chat"]
```

### huggingface

The OpenAI-compatible **Inference router** (`https://router.huggingface.co/v1`),
distinct from the self-hosted `tei` kind. `api_key_env` holds a Hugging Face
token; `upstream_id` is a routed model id (often `owner/model:provider`).

```toml
[[providers]]
name = "hf"
kind = "huggingface"
api_key_env = "HF_TOKEN"
[[providers.models]]
id = "qwen"
upstream_id = "Qwen/Qwen2.5-72B-Instruct"
capabilities = ["chat"]
```

### cloudflare

Cloudflare **Workers AI** via its OpenAI-compatible endpoint. `base_url` is
**required** because it embeds your account id; `api_key_env` holds a Cloudflare
API token. (Workers AI reranking uses a Cloudflare-specific endpoint and is not
covered by this OpenAI-compatible kind yet - see `docs/backlog.md`.)

```toml
[[providers]]
name = "cf"
kind = "cloudflare"
api_key_env = "CLOUDFLARE_API_TOKEN"
base_url = "https://api.cloudflare.com/client/v4/accounts/YOUR_ACCOUNT_ID/ai/v1"
[[providers.models]]
id = "cf-llama"
upstream_id = "@cf/meta/llama-3.1-8b-instruct"
capabilities = ["chat"]
```

### vllm

Any self-hosted OpenAI-compatible server (vLLM, llama.cpp `--api`, LM Studio,
SGLang, LocalAI). `base_url` **required**, API key optional.

```toml
[[providers]]
name = "local"
kind = "vllm"
base_url = "http://localhost:8000/v1"
[[providers.models]]
id = "local-llama"
upstream_id = "meta-llama/Llama-3.1-8B-Instruct"
capabilities = ["chat", "embed"]
```

## Vision (image input)

`POST /v1/chat/completions` accepts OpenAI's content-parts message shape, so a
user message can carry text and image parts in one array:

```json
{
  "model": "gpt-4o",
  "messages": [{
    "role": "user",
    "content": [
      { "type": "text", "text": "What is this?" },
      { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KG..." } }
    ]
  }]
}
```

`image_url.url` is either a `data:<media-type>;base64,<payload>` URI (inline
bytes) or a remote `http(s)` URL.

**Per-model opt-in.** A model only accepts image parts once its config
declares the `image` modality (default is `["text"]`):

```toml
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
modalities = ["text", "image"]   # opts this model into vision
```

`GET /v1/models` reflects this back as `"modalities": ["text","image"]` per
model. Sending an image part to a model whose `modalities` lack `"image"` is
rejected with `LM-2003` (400, see `docs/errors.md`) before any upstream call.

**Which kinds support it:**

| Provider family | `data:` (inline base64) | `http(s)` URL |
|---|---|---|
| OpenAI-family (`openai` + the OpenAI-compatible kinds) and `vllm` | forwarded verbatim | forwarded verbatim |
| `anthropic` | translated to a `base64` image source block | translated to a `url` image source block (Anthropic fetches it) |
| `google` (Gemini) | translated to `inline_data` | rejected - `LM-2004` |

**Never-fetch rule.** LUMEN never dereferences a user-supplied image URL
itself - doing so would be an SSRF vector (the gateway could be aimed at
internal addresses) and would violate the streaming/latency pillar. A remote
`http(s)` `image_url` is only ever forwarded to a provider that fetches it
itself (OpenAI, Anthropic); Gemini's `inline_data` field takes only inline
bytes, so a remote URL routed to Gemini is rejected with `LM-2004` (400)
instead of the gateway silently fetching it on the caller's behalf.

The `LM-2004` pre-flight check inspects the **primary** provider of the model's
fallback chain. In the uncommon case where the primary accepts remote URLs
(e.g. OpenAI) but a Gemini model is configured as a *fallback*, a request with a
remote image URL passes pre-flight and, only if the primary then fails over to
Gemini, surfaces as an upstream `LM-3002` (502) - the gateway still never
fetches the URL. Configure inline `data:` URIs when a Gemini fallback is in play.

**Provider-native image sources (issue #12).** Two provider-native reference
forms are recognised in the `image_url.url` field, for callers whose images
are already uploaded to the provider:

| Reference form in `url` | Translated for | Becomes |
|---|---|---|
| `anthropic-file:<file_id>` | `anthropic` | `source: {type: "file", file_id}` (Anthropic Files API) |
| `https://generativelanguage.googleapis.com/...` (Gemini Files API URI) | `google` | `fileData.fileUri` |
| `gs://bucket/object` (Cloud Storage URI) | `google` | `fileData.fileUri` |

A provider-native reference routed to a model whose **primary** provider is
not the reference's own provider is rejected pre-flight with `LM-2008` (400) -
an honest client error instead of a confusing upstream failure. A Gemini
Files API URI is also an `https://` URL, but it is exempt from the `LM-2004`
remote-URL check: it is not a URL the provider would have to fetch, and its
routing verdict belongs to the `LM-2008` check.

> **`gs://` caveat.** The gateway forwards a `gs://` URI to Gemini verbatim,
> but the Gemini **Developer API** (`generativelanguage.googleapis.com`, the
> default `base_url` of the `google` kind) documents `fileData.fileUri` for
> its own Files API URIs; Cloud Storage `gs://` URIs are a **Vertex AI**
> capability. Against the default endpoint a `gs://` reference is passed
> through and will be rejected *by the upstream* (surfacing as an upstream
> error naming `google`). It is still parsed and forwarded because the
> reference form is Gemini-native (mismatch routing stays an honest
> `LM-2008`), `base_url` may point at a Vertex-compatible gateway, and the
> upstream - never the gateway - is the authority on which URI forms it
> accepts. Upload via the Gemini Files API and pass the returned URI when
> targeting the Developer API.

For the mime type of a `fileData` part: it is included only when it can be
confidently inferred from the URI's file extension (`.png`, `.jpg`, ...);
otherwise it is omitted rather than guessed. Files API URIs carry no
extension, and Gemini already knows the mime type recorded at upload time.

**Accounting.** Upstream-reported `usage` already folds in image tokens; when
an upstream reports no usage, the local estimation fallback counts text only
(images contribute `0`) and the response is still flagged `"estimated": true` -
see the [ADR 003 addendum](adr/003-token-accounting.md#addendum-m8--vision--image-input).

## Fallbacks across providers

Any model can name an ordered list of `fallbacks` - models that back it when its
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
