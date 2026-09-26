# Design proposals: a Jev-backed `/v1/rerank` (SystemOne rerank mapping)

- Status: proposal, for discussion. A minimal form of Proposal A (one
  converter per model id, noul strategy only) shipped as the ADR 013
  amendment; the rest is still open
- Date: 2026-09-26
- Builds on: ADR 013 (SystemOne capability), ADR 012 (config source and
  granular admin endpoints), ADR 009 (budget groups)

## The problem

ADR 013 ships `POST /v1/systemone`. The next step is letting a plain
Cohere-format `/v1/rerank` call be answered by Jev, because Jev is a strong,
cheap, calibrated scorer (TypeSafe's own
[re-ranking cookbook](https://docs.typesafe.ai/cookbooks/rerank_typesafe.md)
takes BM25 top-1 accuracy from 5% to 18% with one noul per candidate).

Unlike Cohere or Voyage, Jev has no built-in notion of "relevance". Someone
has to write the question: what counts as a match, in what domain, with which
criteria. That question is the **mapping**. Two things need deciding:

1. **What a mapping is**: how `{query, documents, top_n}` becomes a SystemOne
   request, and how answers become `relevance_score`s.
2. **How a mapping is selected** for a request: fixed per model, per key or
   tenant, or per request.

The second question is the one that matters for a multi-tenant deployment
(e.g. Meilisearch fronting many customers, each wanting relevance defined its
own way), and it is where the proposals below differ.

## Part 1: what a mapping is (common to every proposal)

A mapping, call it a **rerank profile**, is a small, operator-authored
object:

```toml
[systemone.rerank_profiles.legal-precedent]
target = "jev"               # a SystemOne model id: pricing, fallbacks, breakers come from it
strategy = "noul"            # noul | score | choice | composite
instructions = "Could `candidate` be the precedent cited in the query?"
criteria.true = "The candidate states the specific rule the query cites."
criteria.false = "The candidate is only on a similar topic."
# optional, static domain context injected into the state for every call
context = "US federal case law. Prefer holdings over dicta."
```

### Encoding strategies

| Strategy | SystemOne shape | `relevance_score` | Good for | Limits |
| --- | --- | --- | --- | --- |
| `noul` (default) | one noul per document | the noul (probability of yes) | "is this a match?"; calibrated, so an absolute cut-off like 0.5 means something | none beyond context size |
| `score` | one score per document, graded levels ("off-topic" to "fully answers") | `score / (levels - 1)` | graded relevance, fewer near-ties | 10 levels max |
| `choice` | ONE choice, options = document indices | the option's probability | "which single passage answers this?" (listwise; scores sum to 1) | 255 documents max; scores are relative, not absolute |
| `composite` | several nouls per document ("on topic", "answers the question", "authoritative") | weighted sum, weights in the profile | relevance that is really several judgments | question count grows with documents x criteria |

### Request packing

The query goes in the `state` once; each document goes in its own question,
using TypeSafe's structured instructions:

```json
{
  "model": "jev-latest",
  "state": { "query": "<query>", "context": "<profile context>" },
  "questions": {
    "d0": { "type": "noul",
            "instructions": { "candidate": "<document 0>",
                              "question": "Could `candidate` be the precedent cited in the query?" },
            "criteria": { "true": "...", "false": "..." } },
    "d1": { "...": "same template, document 1" }
  }
}
```

That is one upstream call for the whole batch. Jev evaluates questions
independently, so documents never see each other, and the query is billed
once rather than once per document (the cookbook's one-call-per-pair form
repeats it). Jev's budget is 64k tokens per request and 32k for the state plus
the longest question, so the adapter splits the documents into several
concurrent calls when the estimate runs past a safety margin (the same
fan-out pattern `embed_batched` uses), and rejects a single document that
cannot fit on its own with a clear `LM-1001` rather than truncating it.

### Where the code lives

An adapter, `SystemOneRerankProvider`, implements `RerankProvider` by
wrapping an `Arc<dyn SystemOneProvider>` plus a compiled profile. The rest of
`/v1/rerank` (validation, ordering, `top_n`, `return_documents`, admission)
stays as it is. Accounting reports Jev's `input_tokens` as rerank
`usage.total_tokens` (upstream-reported, unflagged). Search units stay
derived. Cost needs one small addition: today rerank is priced only through
`cost_per_1k_searches`, so a Jev-backed rerank model must be priced by the
tokens of its target (`cost_per_1m_input` on the target, or on the rerank
model itself).

## Part 2: how a mapping is selected

### Proposal A: the mapping is a model ("custom rerank model")

Each profile is exposed as its own rerank model id:

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

  [[providers.models]]
  id = "legal-rerank"              # what clients send as `model`
  capabilities = ["rerank"]
  rerank_profile = "legal-precedent"
  fallbacks = ["cohere-rerank"]    # a Jev-backed rerank can fall back to Cohere
```

The client picks the mapping by picking the model. Per-tenant mappings are
per-tenant model ids (`tenant-a-rerank`, `tenant-b-rerank`), created at
runtime through ADR 012's `PUT /admin/config/providers/{name}` in DB mode.

- **Request path**: unchanged. The profile is compiled into the registry at
  build/reload time (ArcSwap), so there is no lookup per request.
- **Pros**: no new concept; `/v1/models`, pricing, fallbacks, circuit
  breakers, hot reload and `usage_log.model` all just work; debugging is
  trivial because the model id names the mapping.
- **Cons**: the client must know its tenant's model id. Nothing stops key A
  from sending tenant B's model id, since LUMEN has no key-to-model scoping
  today; that is harmless when mappings are not sensitive, less so when they
  encode proprietary rules. The model namespace grows linearly with tenants.

### Proposal B: A plus a per-group (or per-key) model alias map

Keep A for defining mappings, and add one generic routing layer: a budget
group (ADR 009) or a key carries a small alias map, applied before routing:

```json
PATCH /admin/groups/tenant-a
{ "model_aliases": { "rerank": "tenant-a-rerank", "jev": "jev-1.13.0" } }
```

Every tenant's client sends the same model id (`"rerank"`), and the gateway
rewrites it according to the authenticated key's group, then routes as usual.

- **Request path**: one `ArcSwap` load plus a `HashMap` hit on the key/group
  entry the auth middleware already holds. No database access, well inside
  the 1 ms budget. Everything downstream stays static (A's compiled profiles).
- **Pros**: clients need no tenant awareness, which is exactly the Meilisearch
  case (one embedder/reranker config, many customers behind Meilisearch-owned
  keys). The mapping follows the credential, so tenant isolation comes by
  construction, and an alias map can also *restrict*: a group with aliases
  configured can be limited to them, which gives LUMEN the key-to-model
  scoping it lacks today. It is generic: the same map gives per-tenant chat
  or embedding models for free.
- **Cons**: the same request can get different answers depending on the key,
  which is powerful but opaque. It must be visible: the `x-lumen-model-used`
  header already names the model that served, and `usage_log` should record
  the requested model and the resolved one. It needs a SQLite migration
  (a group/key column), admin API fields, and its own ADR.

### Proposal C: the request picks a profile by name

The client names an operator-defined profile in a header, e.g.
`x-lumen-rerank-profile: legal-precedent`, next to a generic Jev-backed rerank
model. Optionally a key or group lists the profiles it may use.

- **Pros**: no model-id explosion; the caller chooses per call (e.g.
  Meilisearch could pick a profile per index from index settings).
- **Cons**: LUMEN-specific header, so stock Cohere clients cannot use it; the
  selection is invisible in the body; "profile not allowed for this key" is a
  new error path; it overlaps with B while being less general.

### Proposal D (rejected): the mapping inline in the request body

Letting a rerank request carry its own `instructions` / `criteria` gives full
flexibility with zero configuration, but it turns `/v1/rerank` into
`/v1/systemone` with worse ergonomics. A caller who wants to author the
question should call `/v1/systemone`, which already exists. It also breaks
Cohere compatibility (other rerank providers would silently ignore the
extension) and moves prompt authoring from the operator to every client.

### Comparison

| | A: model = mapping | B: A + alias map | C: profile header | D: inline |
| --- | --- | --- | --- | --- |
| Client changes | send tenant's model id | none | send a header | send the mapping |
| Per-tenant without client awareness | no | **yes** | no | no |
| Stock Cohere clients | yes | yes | no | no |
| New request-path work | none | 1 map lookup | 1 map lookup | parse + compile per call |
| Tenant isolation | by convention | **by credential** | by allowlist | none |
| New admin/schema surface | none (ADR 012 covers it) | group/key field + migration | profile allowlist | none |
| Reusable beyond rerank | no | **yes** (any capability) | no | no |

## Recommendation

1. **Build A first.** It is the "custom rerank model defined in config" and
   is useful on its own: one operator, a handful of profiles, Jev as a
   reranker with a Cohere fallback. It needs no new admin surface, since
   ADR 012 already manages models at runtime.
2. **Follow with B as its own ADR**, framed as generic per-group model
   aliasing rather than a rerank feature. It solves per-tenant rerank and
   also closes the key-to-model scoping gap that the Meilisearch-gateway
   review flagged.
3. Hold C until a caller really needs per-call selection that B cannot give.
   Reject D.

## Open questions for you

1. **Who authors mappings?** Only the operator (A/B/C), or should tenants
   self-serve through some admin surface? That decides whether profiles need
   ownership, quotas on size, and validation against prompt abuse.
2. **Is a mapping sensitive?** If tenants' relevance rules are proprietary,
   B's credential-bound resolution (with restriction) becomes a requirement,
   not a nice-to-have.
3. **Score semantics.** Jev's noul is calibrated. Do we want to expose an
   absolute cut-off (e.g. a LUMEN `min_relevance` extension that drops
   documents below 0.5), or keep strict Cohere semantics and leave
   thresholds to the caller?
4. **Default strategy.** `noul` per document is the safe default. Is `choice`
   ("which one passage answers?") worth shipping in v1, given it caps at 255
   documents and yields relative scores?
5. **Scope of B.** Group only (simpler, matches ADR 009 tenants), or key-level
   overrides on top of the group?
