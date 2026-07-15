# ADR 003 - Token accounting for every request, every capability

- Status: accepted (core promise)
- Date: 2026-07-12

## Context

Token counting is a headline reason to run LUMEN: an operator fronting many
providers wants one trustworthy answer to "how many tokens did this cost,
per model / key / team / capability?" - without trusting each upstream to
report it and without leaking prompts to a third party to find out.

The problem is that upstream usage reporting is **inconsistent**:

- OpenAI/Cohere/Voyage embeddings report input tokens; **TEI reports none**
  (its `/embed` is a bare vector array) - so a naive gateway shows `0` tokens
  for TEI embeddings.
- Streaming chat only carries usage if the client sends
  `stream_options.include_usage` (and some providers still omit it).
- Rerank is billed in **search units** by Cohere but in **tokens** by
  Jina/Voyage, and TEI reports neither.

So "count the tokens for all the APIs" cannot mean "pass through whatever the
upstream says." It means the gateway **guarantees a count for every call**, and
is honest about whether that count is measured or estimated.

The hard constraint is pillar 1: **< 1 ms added p99, no blocking the runtime.**
Real tokenizers (BPE) cost real CPU; running one inline on the request path
would blow the latency budget.

## Decision

Token accounting is a first-class, always-on output of every chat, embeddings
and rerank call, produced by a two-tier strategy and surfaced in three places.

### Source, in priority order
1. **Upstream-reported usage** - authoritative and free (already in the response
   body / final SSE chunk). Always preferred. `estimated = false`.
2. **Local estimation fallback** - when the upstream omits usage. `estimated =
   true`. Two levels:
   - default: a cheap, allocation-light **heuristic** (byte/char-based) that is
     safe to compute anywhere;
   - opt-in: an **accurate tokenizer** (`tokenizers`/tiktoken-style) selected
     per model in config, run via `spawn_blocking` so it never occupies a tokio
     worker.

### Hot-path rule
The request path never runs a heavy tokenizer. Upstream usage is passed through
as-is; when it is missing, accurate estimation happens **off the hot path** in
the async usage-writer task. The response `usage` field carries the
upstream value when present, else the cheap heuristic (flagged `estimated`),
never a blocking BPE pass. Counting must **never fail or slow a request**.

### What is counted, per capability
- **Chat:** `input_tokens` (prompt) and `output_tokens` (completion).
- **Embeddings:** `input_tokens`.
- **Rerank:** `search_units` when the provider reports them, plus a token count
  of `query + documents` for uniform observability (`estimated` when derived).

### Three surfaces
1. **Response body** - OpenAI-compatible `usage` (chat/embeddings) unchanged.
2. **Prometheus** - cumulative counters, low fixed cardinality:
   `lumen_tokens_total{capability, model, provider, direction, estimated}`
   and `lumen_rerank_search_units_total{model, provider}`. Optional
   metadata-allowlist labels come from ADR 002 (never client-unbounded).
3. **`usage_log`** - per-request `tokens_in`, `tokens_out`,
   `search_units`, `estimated`, alongside cost and metadata for later slicing.

## Consequences

- Every call gets a token count regardless of provider - the TEI-reports-nothing
  gap is closed by estimation, and the count is labelled honestly.
- The latency pillar holds: passthrough is free; BPE estimation is off-hot-path
  and on the blocking pool.
- The embeddings and rerank paths already surface upstream-reported `usage`;
  this ADR adds the estimation fallback and the Prometheus/usage_log counters
  plus streaming extraction. Cost counting (§5.4) becomes a consumer of these
  token counts rather than the thing that defines them.
- New error/counter surface: a `tokens_estimated_total` counter lets operators
  see how much of their accounting is measured vs estimated.

## Addendum (M8 - vision / image input)

Image content parts (`{"type":"image_url",...}`) do not change the priority
order above; they sharpen what "estimation" means when tier 2 fires.

- **Upstream usage stays authoritative and untouched.** OpenAI, Anthropic and
  Gemini all fold image tokens into their reported `prompt_tokens`, so a vision
  request with upstream-reported usage is exactly as accurate as a text-only
  one - no special-casing needed.
- **The local estimation fallback counts text plus a flat per-image estimate.**
  When the upstream omits usage, the heuristic estimator (`estimate_chat_prompt`,
  `crates/core/src/tokens.rs`) sums `MessageContent::text()` per message (the
  concatenation of `text` parts) plus, for every `image_url` part, a flat
  per-image token constant chosen from the part's `detail` hint:
  `"low"` -> `85` tokens (OpenAI's exact, resolution-independent low-detail
  cost - no dimensions needed to reproduce it); `"high"`/`"auto"`/unset ->
  `765` tokens, an approximation of OpenAI's `85 + 170 * tiles` tile formula
  for a typical ~1024x1024 image. The response is still flagged
  `"estimated": true`, so the client is never told a number is measured when
  it is not.
- **A true per-dimension tile count is still deferred** - see
  `docs/backlog.md`. OpenAI's real high-detail formula depends on decoded
  pixel dimensions, which a `data:` URI does not carry and this gateway does
  not extract (the hot-path rule above - never decode/inspect image bytes on
  the request path - still holds). The flat `765`-token constant is a
  documented approximation, not an attempt at per-image precision; it trades
  some accuracy for closing the "silently counts as 0" gap entirely.

## Addendum (issue #10 - rerank token usage shape)

`RerankUsage` (§ "What is counted, per capability") carries `search_units`
and `total_tokens` as two independent counts, each with its own
`*_estimated` flag (`estimated` for `search_units`, `tokens_estimated` for
`total_tokens`) rather than one shared flag - a response can have a real
`search_units` (Cohere) alongside a derived `total_tokens`, or the reverse
(Jina/Voyage). The priority order from the "Source" section above applies
per count: Jina and Voyage report `usage.total_tokens` in their rerank
response, which the gateway passes through unflagged; when a provider omits
it (Cohere, TEI, or Jina/Voyage without `usage`), the gateway derives
`total_tokens` from `query + documents` via the existing byte-heuristic
estimator, flagged `"tokens_estimated": true`.

## Addendum (M9 - multimodal embeddings)

The same priority order applies to image content parts in `/v1/embeddings`:
upstream `usage` is trusted when reported (Cohere, Voyage and Jina all fold
image cost into their reported token/usage counts). When the upstream reports
nothing, the local fallback estimates text parts only (image parts contribute
0 tokens) and the response is still flagged `estimated: true`. This undercounts
image-heavy requests on a no-usage upstream; a per-image token heuristic is a
backlog item (see the ROADMAP M9 note). Media volume itself is accounted
separately (count + decoded bytes) via `lumen_media_total` /
`lumen_media_bytes_total` and the `usage_log` `media_count`/`media_bytes`
columns, not through the token counters.
