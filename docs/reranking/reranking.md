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
  "usage": { "search_units": 1, "total_tokens": 42, "tokens_estimated": true }
}
```

Results come back sorted by descending `relevance_score`. Each result's
`index` points back to the position of that document in the request's
`documents` array, not the sorted position.

## Empty `documents`

`documents` must be non-empty. An empty list is rejected before any upstream
call with `LM-2010` (400). See [Error codes](../errors.md).

## Billing: search units and tokens

Rerank billing units vary by upstream: Cohere and Pinecone bill in **search
units** (one unit is approximately one query over up to 100 documents),
while Jina and Voyage bill in **tokens**. Set `cost_per_1k_searches` on a
model's `[[providers.models]]` block to price the search-unit case:

```toml
[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
cost_per_1k_searches = 2.0
```

Search units are counted on the `lumen_rerank_search_units_total{model,
provider}` Prometheus counter for every request, upstream-reported when
available, otherwise a gateway estimate (`usage.estimated: true`).

Independently of the billing unit, every `/v1/rerank` response also carries a
`usage.total_tokens` count (ADR 003), for uniform token observability across
capabilities: Jina and Voyage surface their upstream-reported token count
unflagged; every other provider (Cohere, TEI, Pinecone, ...) gets a
gateway-derived `query + documents` heuristic flagged
`"tokens_estimated": true`. This token count also feeds
`lumen_tokens_total{capability="rerank",...}` and `usage_log`, alongside the
search-unit accounting - see
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

Like any capability, rerank can fall back across different provider kinds,
through a [virtual model](../virtual-models.md). A three-hop chain across
Cohere, Jina and Voyage survives any single vendor outage:

```toml
[[providers.models]]
id = "cohere/rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]

[[virtual_models]]
id = "rerank-english"
capability = "rerank"
strategy = "fallback"
targets = [{ model = "cohere/rerank-english" }, { model = "jina-rerank" }, { model = "voyage-rerank" }]
```

The foundation model that actually served the request (primary or a fallback)
is reported in the `x-lumen-model-used` response header, and the path taken in
`x-lumen-route`. See [Resilience](../operations/resilience.md).

## Jev as a reranker (TypeSafe)

A rerank [virtual model](../virtual-models.md#7-jev-as-a-reranker) whose
target carries a `remap` is served by Jev. The target points at a `typesafe`
foundation model that declares `systemone` (a `typesafe` model can no longer
declare `rerank` itself). Each request becomes SystemOne calls whose `state`
is `{"query": ...}` and whose questions, by default, are one `noul` per
document, the document carried in structured instructions; Jev's noul (its
calibrated probability that the document is relevant) is the
`relevance_score`.

```toml
[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"

[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone"]
cost_per_1m_input = 0.042

# Optional remap fields: the question asked about every document. Unset
# fields default to a generic relevance question.
[[virtual_models]]
id = "jev-rerank"
capability = "rerank"
strategy = "single"

[[virtual_models.targets]]
model = "jev"
[virtual_models.targets.remap]
strategy = "noul"
instructions = "Could `document` be the precedent cited in the query?"
criteria.true = "The document states the specific rule the query cites."
criteria.false = "The document is only on a similar topic."
```

- Besides `noul`, the remap strategies are `score`, `composite` (weighted
  criteria) and `choice`, and a static `context` can be added; see
  [Virtual models](../virtual-models.md#7-jev-as-a-reranker).
- Clients call plain `/v1/rerank`; ordering, `top_n`, `return_documents`,
  `rank_fields` and fallbacks (a `fallback` virtual model whose second target
  is `cohere/rerank-english`, say) work as for any reranker.
- Documents are packed into as few upstream calls as Jev's context allows (at
  most 100 documents and about 48k estimated tokens per call), run up to 4 at
  a time. A document longer than about 4,096 tokens is truncated first, like
  Cohere's default `max_tokens_per_doc`.
- Because scores are calibrated probabilities, they are comparable across
  requests: a cut-off such as 0.5 means the same thing everywhere.
- Usage: Jev's upstream `input_tokens` is `usage.total_tokens` (unflagged);
  search units are derived. The SystemOne model's `cost_per_1m_input` bills
  per input token.
- The remap belongs to a target of a virtual model. For per-tenant relevance
  rules, put a `switch` on the budget group in front of several such virtual
  models.

## Providers

Which provider kinds serve `rerank` and their setup is in
[Providers](../providers.md).
