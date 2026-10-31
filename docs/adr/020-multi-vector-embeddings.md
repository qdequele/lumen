# ADR 020 - Multi-vector (late-interaction) embeddings

- Status: proposed
- Date: 2026-10-31
- Amends: ADR 014 (new `switch` fact `input_type`)

## Context

Late-interaction models return one vector per input token instead of one
vector per input; a store scores a query against a document with MaxSim over
the two vector sets (the ColBERT family). In October 2026 Perplexity released
`pplx-embed-v2-late` (0.6B and 9B, MIT, text or image, 128-dim vectors). The
two sizes share one embedding space, so a cheap 0.6B query encoder can search
an index built with the 9B model. Jina already serves multi-vector output from
its hosted API (`jina-embeddings-v4` with `return_multivector: true`, and
`jina-colbert-v2`).

Today every LUMEN embedding is one `Vec<f32>` per item
(`crates/core/src/embed.rs`), which batching, `encoding_format` and the
response type all rely on. No other gateway we surveyed (OpenRouter, LiteLLM,
Vercel AI Gateway, Portkey, Bifrost) documents multi-vector output, so the
only wire format in use is Jina's: request `return_multivector` and `task`,
response `data[i].embeddings` (an array of vectors, or one base64 string per
vector).

`pplx-embed-v2-late` has no hosted API and no vLLM or TEI support; it runs only
under sentence-transformers (`MultiVectorEncoder`). Something has to serve it
over HTTP.

## Decision

| # | Decision |
|---|---|
| D1 | **Opt-in per request on `POST /v1/embeddings`** with `return_multivector: true` (Jina's name). Without it the response is unchanged, so an OpenAI client never receives a nested array. No new endpoint. |
| D2 | **Response follows Jina's `MultiEmbeddingData`**: `data[i] = {"object": "embedding", "index": i, "embeddings": [...]}` with one entry per vector. `encoding_format: "float"` gives arrays of floats; `"base64"` gives one base64 string of little-endian `f32` per vector (the single-vector encoding, per vector). **The default is `base64`** for multi-vector requests (about half the size of JSON floats); `"float"` must be asked for. `usage` is unchanged. The gateway never scores (MaxSim is the store's job). |
| D3 | **Models declare their output shapes**: per-model `embedding_outputs`, a subset of `["single", "multi"]`, default `["single"]`. `jina-embeddings-v4` declares both, `pplx-embed-v2-late-*` declares `["multi"]`. Exposed in `GET /v1/models`. The capability stays `embed` (no new capability, so virtual models, metric labels and budgets are untouched). |
| D4 | **Shape enforcement before any upstream call.** A request whose shape (multi when `return_multivector` is true, else single) is not declared by **every** model in the resolved chain fails with **`LM-2014` (400)** naming the first such model, the same all-attempts rule as `LM-2003` (M9), so a fallback never changes the response shape. |
| D5 | **`input_type` is required with `return_multivector`** and must be `search_query` or `search_document` (the existing vocabulary, `crates/providers/src/cohere`). Missing or any other value is `LM-1001`. A query encoded as a document degrades ranking with no error, so the gateway refuses to guess. |
| D6 | **Upstream wire protocol = a subset of Jina's v4 embeddings API**, served by the existing **`jina` provider kind**: `{model, input, task, return_multivector: true, embedding_type}`, with `search_query` mapped to `task: "retrieval.query"`, `search_document` to `"retrieval.passage"`, and `encoding_format` forwarded as `embedding_type`, so the upstream produces the final encoding. One code path covers hosted Jina and any self-hosted server speaking the protocol (`base_url`). Config validation refuses `"multi"` in `embedding_outputs` on any other kind. |
| D7 | **One modality per multi-vector item.** An item is text or an image, never both (`pplx-embed-v2-late` cannot fuse them, and Jina v4 takes one per element). A mixed item is `LM-1001` for multi-vector requests instead of the single-vector Jina rule (image kept, caption dropped), because silently losing the caption is worse for an index that keeps every token. |
| D8 | **No float parsing on the request path.** A multi-vector item's `embeddings` is held as raw JSON (`Box<RawValue>`); reassembling sub-batches rewrites only `index`. The gateway never transcodes between float and base64. |
| D9 | **Bounded size.** A multi-vector request carries at most `max_multivector_items` items (default 64, `LM-1001` above it, checked before any upstream call) and is sub-batched by a per-model `max_batch_size` override (the open backlog item, default 8 for multi-vector requests). Each upstream success body is read streamed under `[embeddings] max_multivector_response_bytes` (default 64 MiB); over the cap the read is aborted and the attempt fails with **`LM-3014` (502)**, a retryable-by-fallback upstream error. Single-vector responses keep today's unbounded read. |
| D10 | **Query and document can use different models.** `input_type` becomes a request fact for virtual-model `switch` rules (amends ADR 014), so one virtual id can send `search_query` to `pplx-embed-v2-late-0.6b` and `search_document` to `pplx-embed-v2-late-9b`. |
| D11 | **A reference server ships in the repo**: `sidecars/late-interaction/`, about 100 lines of Python around sentence-transformers `MultiVectorEncoder`, plus a Dockerfile, speaking the D6 protocol for any `MultiVectorEncoder` model (`pplx-embed-v2-late-0.6b`, `-9b`). It sits outside the Cargo workspace, the release binary and the LUMEN image, has its own CI job (lint plus a protocol test against a stub encoder) and is documented as a reference, not a supported production component. LUMEN's own tests use wiremock, never the server. |
| D12 | **Token accounting is unchanged** (ADR 003): upstream `usage.prompt_tokens` when reported, else the local text estimate flagged `estimated`. Image parts still count as 0 in the fallback (the open M9 gap). A vector-count metric is a follow-up. |

## Consequences

- New request field `return_multivector`; new model field `embedding_outputs`;
  new config knobs `max_multivector_items`,
  `[embeddings] max_multivector_response_bytes` and the per-model
  `max_batch_size`; new codes `LM-2014` and `LM-3014` in `docs/errors.md`;
  `docs/openapi.yaml` gains the multi-vector response variant.
- Hosted `jina-embeddings-v4` multi-vector output works through LUMEN with no
  extra provider code once the `jina` translation exists.
- Python enters the repository, confined to `sidecars/` with its own CI job.
  The reference server has no auth of its own; it must sit on a private
  network (LUMEN sends the provider `api_key` if one is configured).
- Multi-vector payloads are large (a 512-token text item is 512 vectors,
  about 350 KB in base64), so throughput is bounded by D9 rather than by the
  gateway; the < 1 ms overhead target holds because D8 keeps the gateway to a
  byte copy per item.
- Consumers must store and score multi-vectors (Qdrant, Weaviate, Vespa,
  LanceDB). Meilisearch has no MaxSim scoring today, so this does not yet
  serve the main LUMEN consumer.

## Alternatives rejected

- **A separate `/v1/multi-vector` endpoint** (Jina's ColBERT v2 style):
  duplicates auth, budgets, routing and accounting for what is one flag, and
  Jina itself moved to a flag on `/v1/embeddings` for v4.
- **A new `late` provider kind**: Jina's format already carries everything
  needed; a second format would split clients for no gain.
- **Parsing into `Vec<Vec<f32>>`**: allocates and re-serializes megabytes per
  response on the request path, against the performance pillar.
- **Documenting the protocol only, no reference server**: keeps the repo pure
  Rust, but nobody could use the feature until someone wrote that server.
- **vLLM token embeddings as the upstream**: unverified for this model; it can
  be added later as another server speaking D6.
- **Only exposing late interaction as a reranker** (`/v1/rerank`, scoring with
  MaxSim on the server): cheaper and needs no API change, but it cannot build
  an index. It stays a possible follow-up next to this ADR.
