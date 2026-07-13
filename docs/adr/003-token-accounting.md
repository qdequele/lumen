# ADR 003 — Token accounting for every request, every capability

- Status: accepted (core promise; wired across M4–M5)
- Date: 2026-07-12
- Milestones: M4 (streaming extraction), M5 (counters, storage, estimation)

## Context

Token counting is a headline reason to run LUMEN: an operator fronting many
providers wants one trustworthy answer to "how many tokens did this cost,
per model / key / team / capability?" — without trusting each upstream to
report it and without leaking prompts to a third party to find out.

The problem is that upstream usage reporting is **inconsistent**:

- OpenAI/Cohere/Voyage embeddings report input tokens; **TEI reports none**
  (its `/embed` is a bare vector array) — so a naive gateway shows `0` tokens
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
1. **Upstream-reported usage** — authoritative and free (already in the response
   body / final SSE chunk). Always preferred. `estimated = false`.
2. **Local estimation fallback** — when the upstream omits usage. `estimated =
   true`. Two levels:
   - default: a cheap, allocation-light **heuristic** (byte/char-based) that is
     safe to compute anywhere;
   - opt-in: an **accurate tokenizer** (`tokenizers`/tiktoken-style) selected
     per model in config, run via `spawn_blocking` so it never occupies a tokio
     worker.

### Hot-path rule
The request path never runs a heavy tokenizer. Upstream usage is passed through
as-is; when it is missing, accurate estimation happens **off the hot path** in
the async usage-writer task (M5). The response `usage` field carries the
upstream value when present, else the cheap heuristic (flagged `estimated`),
never a blocking BPE pass. Counting must **never fail or slow a request**.

### What is counted, per capability
- **Chat:** `input_tokens` (prompt) and `output_tokens` (completion).
- **Embeddings:** `input_tokens`.
- **Rerank:** `search_units` when the provider reports them, plus a token count
  of `query + documents` for uniform observability (`estimated` when derived).

### Three surfaces
1. **Response body** — OpenAI-compatible `usage` (chat/embeddings) unchanged.
2. **Prometheus** — cumulative counters, low fixed cardinality:
   `lumen_tokens_total{capability, model, provider, direction, estimated}`
   and `lumen_rerank_search_units_total{model, provider}`. Optional
   metadata-allowlist labels come from ADR 002 (never client-unbounded).
3. **`usage_log`** (M5) — per-request `tokens_in`, `tokens_out`,
   `search_units`, `estimated`, alongside cost and metadata for later slicing.

## Consequences

- Every call gets a token count regardless of provider — the TEI-reports-nothing
  gap is closed by estimation, and the count is labelled honestly.
- The latency pillar holds: passthrough is free; BPE estimation is off-hot-path
  and on the blocking pool.
- M2/M3 already surface upstream-reported `usage`; this ADR adds the estimation
  fallback and the Prometheus/usage_log counters (M5) plus streaming extraction
  (M4). Cost counting (M5 §5.4) becomes a consumer of these token counts rather
  than the thing that defines them.
- New error/counter surface: a `tokens_estimated_total` counter lets operators
  see how much of their accounting is measured vs estimated.
