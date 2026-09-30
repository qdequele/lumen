# ADR 014 - Virtual models

- Status: accepted
- Date: 2026-09-30
- Supersedes: the open proposals in `docs/design/systemone-rerank-mapping.md`
- Builds on: ADR 003, ADR 005, ADR 009, ADR 012, ADR 013

## Context

Today a `[[providers.models]]` entry does four jobs at once: it is the
upstream binding (provider, `upstream_id`), the public id clients send, the
pricing record, and the home of routing logic (`fallbacks`, and the Jev
`[providers.models.rerank]` converter from the ADR 013 amendment). Mixing the
jobs makes each new behaviour (weighted split, conditional routing, request
templates, Jev answering `/v1/rerank` with per-tenant relevance rules) a new
field on a model that is supposed to be a plain binding, and it makes the
logic impossible to compose: a fallback chain cannot point at another chain,
and a Jev reranker cannot fall back to a classic reranker.

The goals are:

1. One place for routing logic. Foundation models become pure upstream
   bindings.
2. Zero measurable cost for requests that use no transform: the routing
   decision stays well under 1 us.
3. Jev as a first-class reranker, including per-tenant relevance rules (via
   `switch` on the budget group) and a cross-capability fallback to a classic
   reranker.
4. Every served request still reports tokens and cost against the foundation
   model that actually served it (ADR 003).

## Decision

1. **Two layers.** Foundation models (`[[providers.models]]`) are pure
   upstream bindings: `id`, `upstream_id`, `capabilities`, `modalities`,
   prices, `release_date`. Virtual models (`[[virtual_models]]`) carry the
   logic. They live in the dynamic config document (ADR 012): config file or
   `config_versions` row, hot-reloaded, editable through `/admin/config`.
   Both layers are callable by clients. They share **one id namespace**
   (allowed characters `[A-Za-z0-9._:/-]`, non-empty); a collision is a
   validation error naming both definitions. Circuit breakers stay keyed by
   (provider, model), as in ADR 005.

2. **One capability and one strategy per virtual model.** A virtual model
   serves exactly one of `chat`, `embed`, `rerank`, `systemone`, which keeps
   remaps and validation unambiguous and makes `GET /v1/models` truthful.
   The strategy is one of:
   - `single`: the one target.
   - `fallback`: targets in order; move on when the failure matches
     `fallback_on`.
   - `split`: pick one target at random by integer `weight`; on a matching
     failure try the rest in descending weight order (ties keep declaration
     order). No stickiness.
   - `switch`: the first target whose `when` matches is used. A chosen branch
     never falls through to the next rule; its resilience comes from what it
     points at.

   Targets are foundation ids or other virtual ids (composition by
   reference). Load-time validation: references exist; strategy
   shape (`single` has 1 target, `fallback` and `split` at least 2, `switch`
   has `when` on every target but the last and none on the last, `when` only
   under `switch`, `weight` only under `split`, `fallback_on` only on
   `fallback`/`split`, `preset` only on `chat`); capability compatibility (or
   a `remap`); no cycles (the error prints the path, e.g. `a -> b -> a`);
   nesting depth at most 8; at most 64 flattened attempts per request
   (`MAX_ATTEMPTS`: a `fallback` or `split` sums its targets, a `switch`
   takes its largest branch, a foundation target is 1), so a DAG listing the
   same child several times per level cannot blow up the decide step;
   contents of `overrides`, `preset` and `remap`.
   Validation runs at boot, reload and on every admin write. `listed = false`
   hides a virtual model from `GET /v1/models` while it stays callable.

3. **Typed fallback triggers (`fallback_on`).** Names: `provider_error`
   (retryable upstream 5xx including TypeSafe 529 Overloaded, an unreachable
   upstream, premature stream end before the first byte; a malformed response
   and non-retryable upstream statuses never trigger a fallback), `rate_limited`
   (upstream 429), `timeout` (connect, first-token and per-attempt timeouts),
   `circuit_open`, plus the opt-in `context_length` and `content_filter`. A
   TypeSafe 529 is a retryable 5xx (ADR 013), so it matches `provider_error`,
   not `rate_limited`. The default when omitted is
   `["provider_error", "rate_limited", "timeout", "circuit_open"]`, exactly
   the ADR 005 behaviour. `context_length` and `content_filter` need each
   provider to classify its upstream error into a new `ProviderError`
   variant; a provider that cannot classify leaves the error as a plain
   client error and nothing fails over (first release: OpenAI and compatible
   hosts, Anthropic, Google/Vertex, Bedrock, Mistral, Cohere). Never a
   trigger: other client 4xx errors, a client cancellation (never touches a
   breaker, ADR 006), or any failure after the first streamed byte
   (ADR 004/005). Retries still happen inside each leaf first; the
   `[resilience].total_timeout_ms` deadline bounds the whole tree.

4. **`switch` conditions (`when`).** Attributes: `group` (the key's budget
   group, ADR 009), `metadata.<key>` (from `x-lumen-metadata`, ADR 002),
   and request facts `has_images`, `has_tools`, `stream`, `input_tokens`
   (the ADR 003 estimate, computed lazily only when a rule references it) and
   `documents` (rerank). Not the virtual key id. Operators: bare value
   (equality), `in`, `ne`, `gt`, `gte`, `lt`, `lte`, `regex` (compiled at
   load). Keys in one `when` are ANDed; `any = [ ... ]` is a nestable OR. A
   missing attribute makes its comparison false; an attribute the capability
   never has is a validation error.

5. **Per-target overrides.** `overrides = { set, default, drop }` on a target,
   applied to the parsed request struct (not JSON paths) against a
   per-capability allow-list of fields (chat sampling and limit fields,
   embed `dimensions` and `encoding_format`, rerank `top_n`, none for
   systemone). Unknown fields, fields outside the allow-list and wrong types
   are validation errors. Order: the requested virtual model's `preset`
   first, then target overrides from the outermost virtual model inward, so
   the level closest to the foundation leaf wins.

6. **Chat presets.** `preset = { system_prompt, system_prompt_mode,
   overrides }` on a chat virtual model. Modes: `prepend` (default),
   `replace`, `if_absent`. The prompt is at most 32 KB, not blank, and counts
   in tokens and cost. A preset only applies when its own virtual model is
   the requested id; on a composed model it is ignored and validation warns.

7. **SystemOne rerank remap.** A `remap` on a target of a rerank virtual
   model, pointing directly at a foundation model that serves `systemone`,
   converts `/v1/rerank` into SystemOne calls. Strategies: `noul` (default,
   one noul per document), `score` (one score per document, normalised
   `level / (levels - 1)`), `composite` (1 to 8 weighted noul questions per
   document, weighted mean), `choice` (one question whose options are the
   documents; must fit one call, at most 255 documents, else `LM-1001`
   before any upstream call). A static operator-authored `context` is sent in
   the state next to the query. Batching and truncation follow the ADR 013
   amendment. Accounting: `usage.total_tokens` is the sum of Jev's reported
   `input_tokens`, cost uses the SystemOne model's `cost_per_1m_input`.
   Preset and remap text is operator-authored config: never client-supplied,
   never logged, never echoed in errors.

8. **Compiled at load, executed per attempt.** On boot and reload the virtual
   models compile into a plan held in the resilience policy's ArcSwap
   snapshot (replacing today's `fallbacks` map); an in-flight request keeps
   the snapshot it started with. Leaves resolve through the provider registry
   by foundation id at request time; a leaf the registry no longer knows is
   skipped (the primary's miss is still the routing error), never a 500. The
   decide phase is pure (no I/O, no allocation beyond a small inline vector):
   it walks `Switch` and `Split` nodes and flattens the result into an
   ordered attempt list, each attempt carrying its governing trigger set and
   route path. The existing ADR 005 executor then runs the list; its only
   change is that moving to the next attempt is decided by that attempt's
   trigger set. The `CancellationToken` flows unchanged and a remap's
   concurrent Jev calls share it. A criterion bench targets under 1 us for a
   3-level plan containing a regex rule.

9. **Old fields removed, migration provided.** `fallbacks` on a model, the
   `[providers.models.rerank]` block, and a `typesafe` model declaring
   `rerank` are validation errors that name the model, point to
   `lumen config migrate` and print the converted snippet. `lumen config
   migrate` rewrites old configs so clients keep working unchanged: a model
   `M` on provider `P` with `fallbacks` becomes foundation `P/M` plus a
   virtual `M` (`fallback` over `P/M` and the old fallbacks); a `typesafe`
   rerank model becomes a virtual rerank model with a `noul` remap onto the
   provider's SystemOne foundation model. File mode backs up to `<file>.bak`
   and writes atomically; DB mode writes a new `config_versions` row;
   `--dry-run` prints the result.

## Consequences

- **Observability.** New `x-lumen-route` response header with the path taken
  (e.g. `acme/chat>acme/chat-eu>mistral-large`), a new `usage_log.route`
  column (migration 0010), a new Prometheus counter
  `lumen_virtual_model_requests_total{virtual_model, model_used}`, counted for
  served requests (virtual ids are operator-defined, so cardinality is bounded), and
  `virtual_model` and `route` tracing span fields (never prompts, presets or
  remap text). `x-lumen-model-used` and `usage_log.model_used` stay the
  foundation leaf that served, so tokens and cost are still booked to it
  (for a remap, the SystemOne model). `GET /v1/models` lists virtual models
  with `"virtual": true` and their single capability; foundation entries gain
  `"virtual": false`.
- **No new request-path error codes.** Unknown id `LM-2001`, wrong capability
  `LM-2002`, exhausted attempts `LM-3004`/`LM-3020` or the last upstream
  error, invalid config `LM-1001` on an admin write (boot error otherwise).
  New cases are listed in `docs/errors.md`.
- **Admin API.** `/admin/config/virtual_models` (list), and per-id `GET`,
  `PUT`, `DELETE` with the ADR 012 `If-Match` rule, plus a read-only
  `/plan` route that shows the fully resolved tree. Deleting a provider,
  foundation model or virtual model that a virtual model still references is
  `LM-1001`.
- **Breaking change, visible side effect.** Renamed foundation ids change
  `usage_log.model_used` and Prometheus model labels (e.g. `gpt-4o` becomes
  `openai/gpt-4o`). A model with several capabilities keeps only one virtual
  model under its old id (the first declared capability); a call for another
  capability returns `LM-2002` until the operator adds a second virtual model
  (it still works through `P/M`, without fallback). The CHANGELOG flags this
  with the migration steps.
- **Performance.** The request path gains one hash lookup plus O(depth +
  rules) of decide work and drops today's per-request clone of a `Vec<String>`
  of fallback ids. No lock, database access or shared mutable state is added.
- **Non-goals.** Model-driven routing (`classify`, `semantic`, spec 2);
  live-state routing (least-busy, latency or throughput sorting, per-target
  RPM/TPM caps, sticky sessions); per-request routing in the body (OpenRouter
  `models`/`provider`, Portkey inline configs); a global rewrite-rule layer;
  named reusable remap templates (remaps are inline on a target); presets for
  capabilities other than chat; per-key model allow-lists and per-tenant
  BYOK.
