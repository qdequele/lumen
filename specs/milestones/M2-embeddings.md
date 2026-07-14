# M2 - Embeddings: first complete request path

## Objective
`POST /v1/embeddings` works end-to-end with OpenAI and Ollama, automatic batching, propagated cancellation. This is the milestone that establishes ALL the patterns (provider, router, tests) - the most important one in the project.

## Tasks

### 2.1 Registry & router
- [x] `crates/providers/src/registry.rs`: builds provider instances from the config, exposes `get(capability, model_id) -> Option<Arc<dyn ...>>`
- [x] `crates/router`: resolves the requested model → provider, otherwise returns LM-2001 (unknown model) or LM-2002 (model lacking that capability)
- [x] Registry behind `ArcSwap` (preparation for M7 hot reload)

### 2.2 OpenAI provider (embeddings)
- [x] `providers/src/openai/`: shared reqwest client (pool), `embed()` with minimal translation (near-direct passthrough)
- [x] Handling of `encoding_format` (float | base64), `dimensions`
- [x] Error mapping: 401→Upstream fatal, 429→RateLimited(retry_after), 5xx→Upstream retryable
- [x] `max_batch_size()` = 2048 inputs

### 2.3 Ollama provider (embeddings)
- [x] `providers/src/ollama/`: `/api/embed` API, Ollama ↔ internal schema translation
- [x] No API key required (local base_url) - the code must accept providers without auth

### 2.4 Batching
- [x] If `inputs.len() > provider.max_batch_size()`: split, run the sub-batches in parallel (bounded concurrency, default 4), reassemble IN ORDER, sum the usages
- [x] Failure of one sub-batch = failure of the entire request with the error from the offending sub-batch (no partial result in v1)

### 2.5 HTTP handler
- [x] `POST /v1/embeddings`: validation → router → provider → OpenAI-format response
- [x] `CancellationToken` created per request, cancelled when the client connection closes (axum: detection via the body/extension), passed all the way to `reqwest` (via `select!`)

## Acceptance criteria
1. wiremock test: request with 5000 inputs, provider with max_batch 2048 → exactly 3 upstream calls, response with 5000 embeddings in the original order, summed usage.
2. Cancellation test: the client drops the connection during the upstream call → wiremock records the upstream request as interrupted / the token is cancelled before completion (assert on counter + simulated delay with start_paused).
3. Test: unknown model → 404 LM-2001; chat-only model requested for embedding → 400 LM-2002.
4. Test: upstream responds 429 with Retry-After → 429 response to the client with the header propagated and code LM-3001.
5. Test: upstream responds with malformed JSON → 502 LM-3002 (never 500, never a panic).
6. Ollama and OpenAI pass the SAME generic test suite (macro or generic conformance-suite function) - this harness will serve all subsequent providers.

## Pattern to establish (reused everywhere afterwards)
Generic conformance suite: `fn conformance_suite<P: EmbeddingProvider>(provider: P, mock: MockServer)` run for each provider. Every new provider MUST pass it.
