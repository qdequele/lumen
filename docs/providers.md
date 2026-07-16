# Providers

LUMEN ships twenty-six built-in provider kinds - fifteen native integrations
(their own request/response translation, including deployment-routed `azure`
and SigV4-signed `bedrock`) plus eleven **OpenAI-compatible** hosts that reuse
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
| `vertex_ai` |  ✅  |       |        | required (SA JSON) | **required** (GCP region) | - |
| `bedrock`   |  ✅  |       |        | AWS SigV4     | optional       | -                 |
| `cohere`    |  ✅  |  ✅   |   ✅   | required      | optional       | 96                |
| `jina`      |      |  ✅   |   ✅   | required      | optional       | 2048              |
| `voyage`    |      |  ✅   |   ✅   | required      | optional       | 128               |
| `mixedbread`|      |      |   ✅   | required      | optional       | -                 |
| `pinecone`  |      |      |   ✅   | required      | optional       | -                 |
| `nvidia`    |      |      |   ✅   | keyless       | **required**   | -                 |
| `tei`       |      |  ✅   |   ✅   | keyless       | **required**   | 32                |
| `ollama`    |  ✅  |  ✅   |        | keyless       | **required**   | 512               |
| `azure`     |  ✅  |  ✅   |        | required      | **required**   | 2048              |

The `together` kind (in the OpenAI-compatible table below) additionally serves
**rerank** (LlamaRank) natively; see its section for the model config.

OpenAI-compatible hosts (chat + embed via the OpenAI path). The Embed column
reflects what each host actually serves upstream: `groq`, `deepseek`,
`openrouter`, `perplexity` and `xai` expose no `/embeddings` endpoint, so a
model declaring `embed` on those kinds is **rejected at config load** (it
could only ever 404 at request time) - unless the provider sets a custom
`base_url`, which is taken to mean an operator-run proxy that may serve
embeddings:

| `kind`        | Chat | Embed | `api_key_env` | `base_url`   | Default base URL                          |
|---------------|:----:|:-----:|:-------------:|:------------:|-------------------------------------------|
| `groq`        |  ✅  |  no   | required      | optional     | `https://api.groq.com/openai/v1`          |
| `together`    |  ✅  |  ✅   | required      | optional     | `https://api.together.xyz/v1`             |
| `fireworks`   |  ✅  |  ✅   | required      | optional     | `https://api.fireworks.ai/inference/v1`   |
| `deepseek`    |  ✅  |  no   | required      | optional     | `https://api.deepseek.com/v1`             |
| `openrouter`  |  ✅  |  no   | required      | optional     | `https://openrouter.ai/api/v1`            |
| `perplexity`  |  ✅  |  no   | required      | optional     | `https://api.perplexity.ai`               |
| `xai`         |  ✅  |  no   | required      | optional     | `https://api.x.ai/v1`                     |
| `deepinfra`   |  ✅  |  ✅   | required      | optional     | `https://api.deepinfra.com/v1/openai`     |
| `huggingface` |  ✅  |  ✅   | required      | optional     | `https://router.huggingface.co/v1`        |
| `cloudflare`  |  ✅  |  ✅   | required      | **required** | - (URL embeds your account id)            |
| `vllm`        |  ✅  |  ✅   | keyless       | **required** | - (your self-hosted server)               |

Self-hosted or catalog-dependent kinds (`vllm`, `huggingface`, `cloudflare`)
stay permissive: the operator controls what their endpoint serves. If one of
the embed-less hosts above later ships an embeddings API, point a `kind =
"openai"` provider at it with a `base_url` override, or file an issue to
update the capability table.

All embed-serving OpenAI-compatible kinds use a 2048-input embed batch
limit. Anything that speaks the OpenAI wire format but isn't listed can
still be used via `kind = "openai"` with a `base_url` override.

`cloudflare` additionally serves **rerank** (not shown in the table above,
which covers only the chat/embed OpenAI-compatible path): its BAAI
`bge-reranker-*` models are served through Workers AI's native
`/ai/run/{model}` endpoint rather than an OpenAI-compatible one. See
[`### cloudflare`](#cloudflare) below.

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

## vertex_ai

- **kind**: `vertex_ai` · **capabilities**: chat only (Gemini models on Google
  Cloud Vertex AI). Distinct from `google`, which is the public Gemini
  Developer API: Vertex uses regional endpoints and GCP OAuth instead of a
  static API key.
- **Auth**: `api_key_env` names an env var holding the **full service-account
  key JSON** (the contents of the key file downloaded from GCP, not a path and
  not an API key). LUMEN signs an RS256 JWT assertion with the account's
  private key, exchanges it at the account's `token_uri` for a short-lived
  OAuth2 access token (scope `cloud-platform`), and sends it as a `Bearer`
  header. Tokens are cached in memory and refreshed 60 s before expiry, so the
  exchange stays off the per-request hot path. The private key is redacted from
  all `Debug` output and never appears in logs or errors.
- **base_url**: **required** - it carries the **GCP region** (e.g.
  `us-central1`), not a URL. The endpoint is derived from it:
  `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:generateContent`
  (and `:streamGenerateContent?alt=sse` when streaming).
- **Project id**: taken from the service-account JSON's `project_id`.
- **Translation**: identical to `google` (same `GenerateContent` wire schema),
  including streaming. Like Gemini, only inline base64 image data is accepted;
  remote image URLs are rejected with `LM-2004`.

```toml
[[providers]]
name = "vertex"
kind = "vertex_ai"
# The env var holds the service-account key file's JSON contents:
#   export VERTEX_SA_JSON="$(cat service-account.json)"
api_key_env = "VERTEX_SA_JSON"
base_url = "us-central1"   # GCP region

[[providers.models]]
id = "gemini-flash-vertex"
upstream_id = "gemini-2.0-flash"
capabilities = ["chat"]
```

## bedrock

- **kind**: `bedrock` · **capabilities**: chat only.
- **Auth**: AWS Signature Version 4 (SigV4), not a bearer key. Credentials are
  read from the standard AWS environment variables (`AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN` for temporary
  credentials) **on every request**, so values updated in the process
  environment (or a config hot reload) take effect without a restart.
  `api_key_env` is optional and, if set, overrides only the secret access key.
  The secret and session token are never logged or shown in `Debug`.
- **Credential scope (v1)**: only static keys and pre-issued STS session tokens.
  There is no AWS credential-provider chain (no IMDS/instance roles, SSO,
  profiles or `credential_process`); an expired session token keeps failing
  with 403 until the environment supplies a fresh one.
- **API**: the Bedrock **Converse** API (`POST /model/{modelId}/converse` and
  `/converse-stream`), which gives one uniform schema across the Anthropic,
  Meta Llama, Amazon Titan/Nova, Mistral and Cohere model families. The legacy
  per-model `InvokeModel` schemas are intentionally not implemented (Converse
  covers the same models).
- **Region / base_url**: set `base_url` to the runtime endpoint for your region,
  `https://bedrock-runtime.{region}.amazonaws.com`; the region is parsed back out
  of it for the SigV4 signing scope. VPC/PrivateLink endpoint hosts
  (`bedrock-runtime.{region}.vpce.amazonaws.com`, including a `vpce-…`-prefixed
  DNS name) are recognised too. For any other custom endpoint the region comes
  from `AWS_REGION` / `AWS_DEFAULT_REGION`; if no source yields a region, startup
  fails with a clear error rather than silently signing for a wrong region.
- **Translation**: OpenAI ⇄ Converse is bidirectional, including system prompts,
  `inferenceConfig` (max tokens, temperature, top-p, stop sequences), tools, and
  streaming. Streaming arrives as AWS event-stream binary frames, decoded and
  translated to OpenAI chunks. Usage (`inputTokens` / `outputTokens`) is mapped
  per ADR 003.
- **Images**: only inline `data:` URIs are supported (Converse takes raw image
  bytes); a remote image URL is rejected (`LM-2004`) since Bedrock cannot fetch
  one.

```toml
[[providers]]
name = "bedrock"
kind = "bedrock"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
# api_key_env = "AWS_SECRET_ACCESS_KEY"   # optional secret override

[[providers.models]]
id = "bedrock-claude-3-5-sonnet"
upstream_id = "anthropic.claude-3-5-sonnet-20241022-v2:0"
capabilities = ["chat"]
modalities = ["text", "image"]
```

## cohere

- **kind**: `cohere` · **capabilities**: chat, embed, rerank. A single model can
  serve any combination.
- **Auth**: `api_key_env` (e.g. `COHERE_API_KEY`), bearer token.
- **Chat**: Command R / R+ via `POST /v2/chat`, including streaming. The wire
  shape is OpenAI-adjacent (roles live directly in `messages`, no top-level
  `system` hoist like Anthropic; `tool_calls` are already OpenAI-shaped), so
  translation is closer to identity than Anthropic's. `tool_choice` collapses
  to Cohere's `REQUIRED`/`NONE` (forcing one specific named tool has no v2
  equivalent and falls back to `auto`). Usage prefers `usage.tokens` (actual
  counts) over `usage.billed_units` (what's charged); a response reporting
  neither leaves the gateway's local estimator to fill in an honestly-flagged
  count (ADR 003).
- **Vision (issue #73)**: a user message carrying image parts is translated
  to Cohere v2 content blocks (`text` / `image_url`, OpenAI-shaped); a
  text-only message keeps the plain-string form, and non-user roles always
  flatten to text (Cohere only admits image content on user messages). Declare
  `modalities = ["text", "image"]` on a vision model (Command-A-Vision) to
  opt in. Both inline `data:` URIs and remote `http(s)` URLs are forwarded
  (Cohere fetches remote URLs itself, so `LM-2004` does not apply); the
  optional `detail` hint (`low`/`high`/`auto`) passes through untouched.
  Provider-native references (`anthropic-file:`, `gs://`, Gemini Files API
  URIs) are rejected pre-flight with `LM-2008`.
- **Embed batch limit**: 96.
- **Cost**: rerank is billed in search units (`cost_per_1k_searches`).
- **`input_type` override**: Cohere's embed v2 API requires an `input_type`
  and the gateway cannot know query-vs-document intent, so it defaults to
  `search_document` (the indexing case). Set `input_type` as an extra field on
  the `/v1/embeddings` request body to override it per request, e.g.
  `{"model": "embed-multilingual", "input": "...", "input_type": "search_query"}`.
  Allowed values: `search_document`, `search_query`, `classification`,
  `clustering`. An unrecognized value is rejected with `LM-1001` before any
  upstream call. The field is consumed at the gateway: only the Cohere
  translation reads it, and it is never forwarded in the outgoing body of any
  other provider (a strict OpenAI-compatible upstream such as vLLM could
  reject unknown fields).

```toml
[[providers]]
name = "cohere"
kind = "cohere"
api_key_env = "COHERE_API_KEY"

[[providers.models]]
id = "command-r-plus"
upstream_id = "command-r-plus-08-2024"
capabilities = ["chat"]

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

## mixedbread

- **kind**: `mixedbread` · **capabilities**: rerank (hosted, `mxbai-rerank-*`).
- **Auth**: `api_key_env` (e.g. `MXBAI_API_KEY`), bearer token.
- **base_url**: optional; defaults to `https://api.mixedbread.com/v1`.
- **Schema note**: Mixedbread's endpoint is `POST /v1/reranking` (note the
  path: `reranking`, not `rerank`) and renames the request fields (`input`
  instead of `documents`, `top_k` instead of `top_n`) with results nested under
  `data`; the gateway translates transparently.
- **Usage**: billed in tokens, so the gateway reports an `estimated` token count
  (ADR 003) rather than upstream search units.

```toml
[[providers]]
name = "mixedbread"
kind = "mixedbread"
api_key_env = "MXBAI_API_KEY"

[[providers.models]]
id = "mxbai-rerank"
upstream_id = "mixedbread-ai/mxbai-rerank-large-v1"
capabilities = ["rerank"]
```

## pinecone

- **kind**: `pinecone` · **capabilities**: rerank (hosted inference).
- **Auth**: `api_key_env` (e.g. `PINECONE_API_KEY`), sent as the `Api-Key`
  header (**not** a bearer token), alongside a pinned `X-Pinecone-API-Version`
  header the inference API requires.
- **base_url**: optional; defaults to `https://api.pinecone.io`.
- **Schema note**: documents are sent as `{ "text": ... }` objects; only the
  default `text` rank field is used (`rank_fields` selection is out of scope for
  v1).
- **Usage**: Pinecone reports `usage.rerank_units`, carried through verbatim as
  the response's `search_units` (not estimated).

```toml
[[providers]]
name = "pinecone"
kind = "pinecone"
api_key_env = "PINECONE_API_KEY"

[[providers.models]]
id = "pinecone-rerank"
upstream_id = "pinecone-rerank-v0"
capabilities = ["rerank"]
```

## nvidia (NIM)

- **kind**: `nvidia` · **capabilities**: rerank (NVIDIA NIM ranking).
- **Auth**: keyless by default (self-hosted NIMs run without a key); supply
  `api_key_env` (e.g. `NVIDIA_API_KEY`) for the hosted API, sent as a bearer
  token.
- **base_url**: **required** - the NIM root (e.g. `http://localhost:8000` or the
  NVIDIA-hosted ranking endpoint root). The gateway posts to `{base}/v1/ranking`.
- **Schema note**: the request nests `query: { text }` and
  `passages: [{ text }]`; there is no `top_n` on the wire, so the gateway
  requests the full ranking and truncates to `top_n` afterwards (as for TEI).
- **Score semantics**: NIM returns a raw **logit**, passed through unchanged as
  `relevance_score`. Scores are unbounded (can be negative) and are only
  comparable *within a single response*; higher is more relevant. No sigmoid is
  applied.
- **Usage**: NIM reports no token usage, so the gateway reports an `estimated`
  token count (ADR 003).

```toml
[[providers]]
name = "nvidia-nim"
kind = "nvidia"
base_url = "http://localhost:8000"
# api_key_env = "NVIDIA_API_KEY"   # only for the hosted API

[[providers.models]]
id = "nvidia-rerank"
upstream_id = "nvidia/llama-3.2-nv-rerankqa-1b-v2"
capabilities = ["rerank"]
```

## together (rerank)

The `together` kind (see the OpenAI-compatible section for chat/embed) also
serves **rerank** (LlamaRank) natively through Together's Cohere-shaped
`/rerank` endpoint. One `[[providers]]` entry with `kind = "together"` serves
all three capabilities against the same `base_url` and bearer key. Rerank is
billed in tokens, so the gateway reports an `estimated` token count (ADR 003).

```toml
[[providers]]
name = "together"
kind = "together"
api_key_env = "TOGETHER_API_KEY"

[[providers.models]]
id = "llama-rank"
upstream_id = "Salesforce/Llama-Rank-V1"
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

- **kind**: `ollama` · **capabilities**: chat, embed.
- **Auth**: keyless.
- **base_url**: **required** - points at your Ollama server **root** (no
  `/v1`). Embeddings use Ollama's native `POST /api/embed`; chat goes through
  Ollama's OpenAI-compatible endpoint, which lives under `/v1` on the same
  root - the gateway appends the `/v1` itself, so keep `base_url` as the bare
  server root either way.
- **Chat**: served by the shared OpenAI-compatible path - streaming (SSE
  passthrough), cancellation, and token accounting (upstream `usage` when
  Ollama reports it, otherwise a local count marked `estimated`, ADR 003) all
  work exactly as for the `openai` kind.
- **Embed batch limit**: 512.
- **Tip**: a local model may take a while to load into VRAM on its first call -
  relax `first_token_timeout_ms` / `total_timeout_ms` on the provider block (see
  `config.example.toml`). A self-hosted box on a slow link can also override
  `connect_timeout_ms`; note that doing so gives this provider its own (unpooled)
  HTTP client (ADR 005, 2026-07-15 amendment), whereas the first-token and total
  overrides do not. All three fall back to their global defaults when unset.

```toml
[[providers]]
name = "ollama-local"
kind = "ollama"
base_url = "http://localhost:11434"
first_token_timeout_ms = 60000
total_timeout_ms = 120000
connect_timeout_ms = 10000  # optional: own client, relaxed connect deadline

[[providers.models]]
id = "local-llama"
upstream_id = "llama3.2"
capabilities = ["chat"]

[[providers.models]]
id = "nomic-embed"
upstream_id = "nomic-embed-text"
capabilities = ["embed"]
```

## azure

- **kind**: `azure` · **capabilities**: chat, embed. Reuses the OpenAI JSON
  schema verbatim; only the URL, auth, and routing differ from `openai`.
- **Auth**: `api_key_env`, sent as the `api-key` header (never a bearer token).
- **base_url**: **required** - your Azure resource endpoint, e.g.
  `https://<resource>.openai.azure.com` (no shared public default, every
  resource is operator-specific).
- **api_version**: optional - pins the Azure API version sent as the
  `api-version` query parameter on every request (issue #65). For back-compat
  the older form still works: append `?api-version=YYYY-MM-DD` to `base_url`.
  Precedence: the explicit `api_version` field wins over a `base_url` query
  string, which wins over LUMEN's pinned built-in default (see the `azure`
  module doc comment for the exact value). Any query parameters on `base_url`
  other than `api-version` are ignored when building request URLs.
  `api_version` is azure-only: setting it on any other kind is rejected at
  boot.
- **Deployment routing**: Azure routes by URL path
  (`/openai/deployments/{deployment}/...`), not by the `model` field in the
  body. Set each model's `upstream_id` to the **Azure deployment name** - the
  same `upstream_id` mechanism every other kind uses for aliasing already
  carries it through.
- **Embed batch limit**: 2048 (same array-size ceiling as the OpenAI
  embedding models Azure hosts).

```toml
[[providers]]
name = "azure-openai"
kind = "azure"
api_key_env = "AZURE_OPENAI_API_KEY"
base_url = "https://my-resource.openai.azure.com"
api_version = "2024-10-21"

[[providers.models]]
id = "gpt-4o"
upstream_id = "my-gpt4o-deployment"   # the Azure deployment name
capabilities = ["chat"]

[[providers.models]]
id = "azure-embed"
upstream_id = "my-embedding-deployment"
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

Cloudflare **Workers AI**. Chat and embeddings go through its OpenAI-compatible
endpoint; reranking (`bge-reranker-*` models) goes through Workers AI's own
native `POST /ai/run/{model}` endpoint instead, since it is not part of the
OpenAI-compatible surface - one `[[providers]]` entry serves all three
capabilities against the same `base_url`. `base_url` is **required** because
it embeds your account id; `api_key_env` holds a Cloudflare API token.

The native rerank request is `{ query, contexts: [{ text }, ...], top_k }`
(`top_n` is sent as `top_k`); the response is Cloudflare's standard
`{ result: { response: [{ id, score }, ...] }, success, errors }` envelope,
with `id` mapped back onto the original document index. Workers AI reports no
token usage for this model; LUMEN derives a local estimate per ADR 003.

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
[[providers.models]]
id = "cf-rerank"
upstream_id = "@cf/baai/bge-reranker-base"
capabilities = ["rerank"]
```

### vllm

Any self-hosted OpenAI-compatible server (vLLM, llama.cpp `--api`, LM Studio,
SGLang, LocalAI). `base_url` **required**, API key optional. For Ollama,
prefer the native `ollama` kind (chat + embed, see its section above); its
OpenAI-compatible endpoint (`http://localhost:11434/v1`) also works under
this kind, but you lose the native embed path and the `/api/version` health
probe.

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
| `azure` | forwarded verbatim (OpenAI wire schema) | forwarded verbatim |
| `anthropic` | translated to a `base64` image source block | translated to a `url` image source block (Anthropic fetches it) |
| `cohere` | translated to a v2 `image_url` content block (`data:` URI forwarded inline) | translated to a v2 `image_url` content block (Cohere fetches it) |
| `google` (Gemini) | translated to `inline_data` | rejected - `LM-2004` |
| `vertex_ai` | translated to `inline_data` (same as `google`) | rejected - `LM-2004` |
| `bedrock` | translated to a Converse image block (`png`/`jpeg`/`gif`/`webp`) | rejected - `LM-2004` |

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

**Accounting.** Upstream-reported `usage` is authoritative and already folds in
image tokens. When an upstream reports no usage at all, the local estimation
fallback counts each image content part with a flat per-image heuristic (85
tokens at `"detail": "low"`, 765 tokens otherwise) rather than counting it as
zero, and the response is still flagged `"estimated": true` - see the
[ADR 003 addendum](adr/003-token-accounting.md#addendum-m8--vision--image-input).

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
