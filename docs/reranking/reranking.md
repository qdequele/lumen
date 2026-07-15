# Reranking

`POST /v1/rerank` speaks the Cohere request and response format: `query`,
`documents`, `top_n`. The `model` field is one of *your* configured model ids
(the `id` in a `[[providers.models]]` block - see [Providers](../providers.md)).

## Request

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

## Response

```json
{
  "results": [
    { "index": 0, "relevance_score": 0.98 },
    { "index": 1, "relevance_score": 0.02 }
  ],
  "usage": { "search_units": 1 }
}
```

Results come back sorted by descending `relevance_score`. Each result's
`index` points back to the position of that document in the request's
`documents` array, not the sorted position.

## Empty `documents`

`documents` must be non-empty. An empty list is rejected before any upstream
call with `LM-2010` (400). See [Error codes](../errors.md).

## Billing: search units

Rerank is metered in search units, not tokens: one unit is approximately one
query over up to 100 documents. Set `cost_per_1k_searches` on a model's
`[[providers.models]]` block to price it:

```toml
[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
cost_per_1k_searches = 2.0
```

Search units are counted on the `lumen_rerank_search_units_total{model,
provider}` Prometheus counter for every request, upstream-reported when
available, otherwise a gateway estimate. See
[Token accounting & cost](../operations/token-accounting.md).

## One model, two capabilities

A single model id can serve both `embed` and `rerank` if the underlying
upstream model supports both, for example Cohere's `embed-v4.0`:

```toml
[[providers.models]]
id = "embed-multilingual"
upstream_id = "embed-v4.0"
capabilities = ["embed", "rerank"]
```

## Cross-vendor fallback

Like any capability, a rerank model can list `fallbacks` across different
provider kinds. A three-hop chain across Cohere, Jina and Voyage survives any
single vendor outage:

```toml
[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
fallbacks = ["jina-rerank", "voyage-rerank"]
```

The model that actually served the request (primary or a fallback) is
reported in the `x-lumen-model-used` response header. See
[Resilience](../operations/resilience.md).

## Providers

Which provider kinds serve `rerank` and their setup is in
[Providers](../providers.md).
