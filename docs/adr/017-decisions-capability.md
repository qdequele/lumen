# ADR 017 - Decisions, one capability for every decision-model vendor

- Status: accepted
- Date: 2026-10-08
- Supersedes: ADR 013 public format (`/v1/systemone` as the only shape)
- Amends: ADR 014 (a rerank `remap` targets any decision model; strategy `noul` renamed `predicate`)

## Context

Decision models answer typed questions with calibrated probabilities. Since
ADR 013 LUMEN served one vendor (TypeSafe Jev) in its own wire format. In
October 2026 OpenAI (`gpt-6-luna`, `POST /v1/decisions`, own format),
Perplexity (`pplx-decider-*`, TypeSafe format at `/v1/decisions`), Cloudflare
(Clef, Workers AI), Ollama (local Nimble, Clef, Kev) and several
TypeSafe-format vendors (Liquid, Inception, Upstage) shipped decision models.
The TypeSafe format is the de facto standard; OpenAI is the only vendor with
its own format, backed by SDK reach.

## Decision

| # | Decision |
|---|---|
| D1 | One capability, renamed `systemone` to **`decisions`**. |
| D2 | **`POST /v1/decisions`** is the public endpoint. It accepts **both formats**, detected from the body, and answers in the format it received. |
| D3 | **`POST /v1/systemone` is deprecated**: a TypeSafe-format-only alias of `/v1/decisions`, removed one minor release later. |
| D4 | The **core type follows OpenAI's schema** (ordered questions, optional names, images, refusals), widened so text fields may hold structured JSON. |
| D5 | Five existing provider kinds serve decisions in this change: `typesafe` (path now configurable, so it is also the generic kind for Liquid, Inception, Upstage and a self-hosted Kev), and `openai`, `ollama`, `cloudflare` and `perplexity` (new `decisions` capability next to their existing ones; `perplexity` and `cloudflare` are today OpenAI-compatible chat kinds). `decisions` on the existing `openrouter` kind is a follow-up. |
| D6 | The rerank `remap` targets **any decision model**; `noul` strategy renamed `predicate` (alias kept). |
| D7 | Jev rerank prompts stay **byte-identical** to today (structured `{document, question}` instructions). Comparing structured vs plain-text prompts is a follow-up evaluation. |
| D8 | Image support reuses the existing per-model `modalities` (`"image"`) and `LM-2003`. |
| D9 | Same-family calls stay faithful in both directions: a TypeSafe-format request to a TypeSafe-family upstream sends the client's question bodies and `state` verbatim, and the client receives the upstream answers (including `legend` and any unknown fields) verbatim. |

## Consequences

- `capability = "systemone"` becomes `"decisions"` (alias with a boot warning,
  `lumen config migrate` rewrites it); the `GET /v1/models` value and the
  `capability` metric label change (breaking, CHANGELOG).
- `/v1/systemone` is deprecated in 0.6.0 and removed in 0.7.0; it returns
  `Deprecation` and `Link` headers and is counted in
  `lumen_deprecated_requests_total{route="/v1/systemone"}`.
- An incompatible fallback target (image to a text-only model, a choice of
  one option to OpenAI, ...) is skipped before any upstream call; it is not an
  attempt and never a circuit failure.
- TypeSafe-format clients keep receiving TypeSafe's bytes from TypeSafe-family
  upstreams; a refusal from an `openai` target is rendered as
  `{"type": "refusal"}`, which TypeSafe SDKs may not deserialize.

## Alternatives rejected

- One endpoint per vendor format (`/v1/decisions` OpenAI only plus
  `/v1/systemone`): Perplexity and Inception clients POST TypeSafe bodies to
  `/v1/decisions`, so the path cannot decide the format.
- A TypeSafe-shaped core type: cannot carry ordered unnamed questions,
  boolean choice values, images or refusals.
- One codec per TypeSafe-format vendor: they differ only in URL, auth,
  image placement, limits and envelope, which is data (`FamilyProfile`).
