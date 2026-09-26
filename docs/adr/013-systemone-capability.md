# ADR 013 - SystemOne (typed decision) capability and the TypeSafe provider

- Status: accepted
- Date: 2026-09-26 (amended 2026-09-26: Jev as a reranker)

## Context

TypeSafe's Jev is the first "System One" model: instead of generating text it
answers typed questions about a `state` with calibrated probabilities. Its
whole API is one endpoint:

```http
POST https://api.typesafe.ai/v1/systemone
Authorization: Bearer <key>

{ "model": "jev-latest",
  "state": "<string | object | array>",
  "questions": { "<id>": { "type": "noul" | "choice" | "score",
                           "instructions": ..., "criteria": ... } } }
```

The response carries one typed answer per question id (a `noul` probability,
a `choice` with its distribution and `confidence`, or a `score` with its
`legend`, distribution and `confidence`), the versioned `model` that answered,
and `usage.{input_tokens, output_tokens}`. Pricing is per input token only.

None of LUMEN's three capabilities fits. Chat is free text; embeddings are
vectors; rerank is a single relevance score per document with no notion of
named questions, options or levels. Squeezing Jev into one of them would lose
the thing that makes it useful (the typed, multi-question answer) while the
operator still wants the gateway's keys, budgets, quotas, token accounting,
fallbacks and metrics in front of it.

## Decision

1. **A fourth first-class capability, `systemone`.** `Capability::SystemOne`
   serializes as `"systemone"` everywhere a capability string appears
   (`capabilities = [...]` in config, `GET /v1/models`, the Prometheus
   `capability` label, `usage_log.capability`, the admin usage filter). The
   name follows the model class rather than a vendor, so a second System One
   vendor slots in as another provider kind.

2. **`POST /v1/systemone`, wire-compatible with TypeSafe.** The request body
   is TypeSafe's, unchanged; the response is TypeSafe's plus, only when the
   gateway had to estimate, a `usage.estimated: true` flag (ADR 003). The
   TypeSafe SDKs read `TYPESAFE_BASE_URL`, so pointing it at LUMEN (and
   `TYPESAFE_API_KEY` at a LUMEN virtual key) makes an existing Jev client go
   through the gateway with no code change. `GET /v1/models` keeps LUMEN's
   OpenAI shape, so the SDKs' `models.list()` is the one call that does not
   work through the gateway; documented, not papered over.

3. **Validation at the edge, answers passed through.** LUMEN forwards a
   non-2xx upstream as `LM-3003` without its body (secrets and upstream
   detail never reach the client, CLAUDE.md rule 8), which would turn every
   TypeSafe `422` into an opaque 502. So the gateway validates the documented
   request contract itself and answers `LM-1001` with a precise message:
   non-empty `model`, a `state`, a non-empty `questions` map (`LM-2011`
   when empty, the SystemOne twin of `LM-2010`), each question's `type` one
   of `noul` / `choice` / `score` with non-null `instructions`, `choice`
   criteria a non-empty map of at most 255 options, `score` criteria an array
   of at most 10 levels, and no duplicate question ids. Duplicate top-level
   keys are rejected while parsing, so what is validated is exactly what is
   forwarded. An unknown question `type` is rejected: a new
   TypeSafe question type is a LUMEN upgrade, not a silent passthrough.
   `instructions`, `criteria` entries and `state` stay opaque JSON (strings,
   objects or arrays are all legal upstream). Unknown top-level request
   fields and every response field are carried verbatim and in order, so
   LUMEN never has to understand an answer to serve it. The request body is
   shared behind an `Arc`, so a retry or fallback attempt costs a refcount,
   not a copy of the state.

4. **`SystemOneProvider` trait, `typesafe` provider kind.** One method,
   `evaluate(req, cancel)`, with the usual `CancellationToken` contract. The
   `typesafe` kind defaults its base URL to `https://api.typesafe.ai`
   (overridable), authenticates with a bearer key, and maps `429` / `529`
   through the shared status classifier (`529` is a retryable 5xx, so the
   executor retries and falls back as for any overloaded upstream; a
   `Retry-After` on a 429 is honoured). Fallback chains, per-model timeouts,
   retries and circuit breakers apply unchanged, e.g. a pinned `jev-1.13.0`
   falling back to `jev-latest`.

5. **Token accounting (ADR 003).** Upstream `usage.input_tokens` and
   `output_tokens` win when reported. `usage` is parsed leniently: a
   missing, null or malformed block (or a zero input count) never fails a
   billed answer, it just falls through to the estimate. In that case (the
   SDK schema marks `usage` optional) the gateway estimates input as the byte
   heuristic over the raw JSON text of `state` plus every question, which is
   what TypeSafe ingests, and flags it `estimated`; output is estimated as
   zero, since answers are tiny and Jev does not bill them. Admission reserves
   the same input estimate before the call. Cost uses the existing
   `cost_per_1m_input` / `cost_per_1m_output` model prices (Jev: `0.042` /
   `0`), so hard budgets bite with no new pricing field.

6. **Privacy.** `state`, questions and answers are request content: never
   logged, never in `usage_log`, never in errors, exactly like prompts.

## Consequences

- One more route, one more trait, one more registry map; the request path is
  the same shape as `/v1/rerank` (resolve chain, admit, execute, settle), so
  no new latency or locking.
- The edge validation duplicates part of TypeSafe's contract and can drift
  from it. Limits are pinned to the documented values (2026-09) and covered by
  tests; an upstream loosening shows up as a LUMEN rejection, an upstream
  tightening as an `LM-3003`.
- A per-key or per-tenant choice of rerank converter is NOT part of this
  decision; see `docs/design/systemone-rerank-mapping.md`.

## Amendment (2026-09-26): Jev as a reranker

A `typesafe` model that declares `rerank` is served by a converter,
`TypesafeRerankProvider`, which implements `RerankProvider` over the model's
`SystemOneProvider`, so the whole `/v1/rerank` path (validation, ordering,
`top_n`, document echo, admission, fallbacks) is unchanged.

- **Mapping.** `state = {"query": <query>}`; one `noul` question per
  document, id = the document index, with structured instructions
  `{"document": <text>, "question": <converter instructions>}` and the
  converter's `criteria.true` / `criteria.false`. `relevance_score` is the
  noul. One call carries many documents, so the query is billed once per
  call, and Jev evaluates questions independently, so documents never see
  each other.
- **Converter config.** An optional `[providers.models.rerank]` block
  (`instructions`, `criteria.true`, `criteria.false`), each defaulting to a
  generic relevance question. It is operator-authored, per model id, and
  rejected at boot on any other kind or capability. Clients cannot supply it.
- **Packing.** At most 100 documents and about 48k estimated tokens per
  upstream call (under Jev's 64k), up to 4 calls in flight; a document over
  about 4,096 estimated tokens is truncated (Cohere's default), so one
  document always fits the 32k state-plus-question budget.
- **Accounting.** The summed upstream `input_tokens` is rerank
  `usage.total_tokens` (if any call omits usage, the whole count falls back
  to the ADR 003 estimate). Rerank cost becomes search-unit price plus
  `cost_per_1m_input` on those tokens; models priced only per search are
  unaffected.
