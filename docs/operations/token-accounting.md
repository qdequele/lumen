# Token accounting & cost

EVERY request of every capability (chat, embed, rerank) produces a token
count. Never a silent zero: upstream usage is used when a provider reports
it, otherwise the gateway falls back to a local estimate flagged
`"estimated": true`. This is a central reason LUMEN exists, not an
afterthought - see [ADR 003](../adr/003-token-accounting.md).

## Why it has to be guaranteed

Upstream usage reporting is inconsistent across providers: TEI's `/embed`
returns a bare vector array with no usage at all; streaming chat only
carries usage when the client asks for it (and some providers omit it even
then); rerank is billed in search units by Cohere and Pinecone but in
tokens by Jina/Voyage, and TEI reports neither. A gateway that only passes
through whatever the upstream says would show `0` tokens for a large slice
of traffic. LUMEN closes that gap: a count is guaranteed for every call, and
the count is always honest about whether it was measured or estimated.

## Where it surfaces

1. **Response body** - the OpenAI-compatible `usage` object on chat and
   embeddings responses, and `usage.search_units` plus `usage.total_tokens`
   on rerank responses (rerank carries both a search-unit and a token count,
   each independently flagged - `estimated` for `search_units`,
   `tokens_estimated` for `total_tokens` - see
   [Reranking - Billing](../reranking/reranking.md#billing-search-units-and-tokens)).
2. **Prometheus** - `lumen_tokens_total` (every capability, including
   rerank's token count) and `lumen_rerank_search_units_total` (rerank's
   search-unit count), both carrying an `estimated` (or upstream-reported)
   signal. See [Metrics & dashboards](metrics.md).
3. **`usage_log`** (when `[auth]` is enabled) - per-request token counts,
   cost and the `estimated` flag, alongside status and metadata for later
   slicing. See [Usage log & multi-tenant metadata](usage-log.md).

## Cached and reasoning token breakdown

When an upstream reports a token breakdown, the gateway surfaces it instead of
discarding it (issue #99), on the same three sinks and under the same "never
zero, never invented" rule:

- **Response body** - an OpenAI-compatible `usage.prompt_tokens_details`
  (`cached_tokens`) and `usage.completion_tokens_details` (`reasoning_tokens`).
  For Anthropic prompt caching, `cache_read_input_tokens` maps to
  `cached_tokens` (a cache read/hit, same as OpenAI) and
  `cache_creation_input_tokens` rides a distinct `cache_creation_tokens` field
  in `prompt_tokens_details` (a cache write, which has no OpenAI equivalent).
- **Prometheus** - `lumen_token_breakdown_total{capability, model, provider,
  kind}` with `kind` one of `cached`, `reasoning`, `cache_write`. It is a
  subset of `lumen_tokens_total`, split out so cache and reasoning usage can be
  summed on their own.
- **`usage_log`** - the nullable `cached_tokens`, `reasoning_tokens` and
  `cache_write_tokens` columns.

A breakdown is only ever an upstream fact: when the upstream reports none, all
three surfaces omit it (`None`/NULL/no series), and a locally estimated count
carries no breakdown at all.

## How estimation works

Source priority, in order:

1. **Upstream-reported usage.** Authoritative and free - already present in
   the response body or the final SSE chunk. `estimated = false`.
2. **Local estimation fallback**, when the upstream omits usage.
   `estimated = true`. The default is a cheap, allocation-light byte/char
   heuristic that is safe to run anywhere, including the request path; an
   accurate tokenizer is an opt-in, off-hot-path option run via
   `spawn_blocking`.

The hot-path rule holds regardless: the request path never runs a heavy
tokenizer. Upstream usage is passed through as-is; when it is missing, the
cheap heuristic fills in the response `usage` field, flagged `estimated`,
and never blocks or slows the request. Streaming chat estimates the same
way when the upstream sends no usage at all in its final chunk.

**Images.** Upstream-reported usage already folds in image cost (OpenAI,
Anthropic and Gemini all report image tokens as part of `prompt_tokens`),
so a vision request with upstream usage is exactly as accurate as a
text-only one. When the upstream reports nothing, the two capabilities
diverge:

- **Chat** counts each image content part with a flat per-image estimate
  (85 tokens at `"detail": "low"`, 765 tokens otherwise) instead of
  counting it as zero. See [Vision (image input)](../chat/vision.md).
- **Embeddings** still estimates image parts as zero tokens when the
  upstream reports no usage; this undercounts image-heavy requests on a
  no-usage upstream, and a per-image heuristic for embeddings is a backlog
  item. Media volume itself (count and decoded bytes) is tracked
  separately via `lumen_media_total` / `lumen_media_bytes_total` and the
  `usage_log` `media_count`/`media_bytes` columns, not through the token
  counters. See [Multimodal embeddings](../embeddings/multimodal.md).

Either way, a locally-estimated count is always flagged `"estimated": true`
- the client is never told a number is measured when it is not.

## Cost

Per-model prices feed cost accounting and hard budgets:

```toml
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
cost_per_1m_input = 2.5
cost_per_1m_output = 10.0
```

Rerank models price by search unit instead:

```toml
[[providers.models]]
id = "rerank-english"
capabilities = ["rerank"]
cost_per_1k_searches = 2.0
```

A model without prices set costs `0`, so hard budgets never bite on it. See
[Keys, quotas & budgets](keys-budgets.md) for how cost feeds budget
enforcement.
