# Virtual models on the hosted Gateway: catalog, routing and composite pipelines

- Date: 2026-11-05
- Status: proposal (research and design only, nothing implemented)
- Repos: Lumen (engine), meilisearch-lab (`saas/` proxy, `console/` admin UI, `contracts/`)
- Builds on: ADR 005 (resilience), ADR 009 (groups), ADR 012 (config source),
  ADR 014 (virtual models), ADR 015 (Lab integration), ADR 017 (decisions),
  ADR 018 (BYOK and bandwidth)

## 1. Why

On the hosted Gateway, a customer should be able to call one slug, for example
`meilisearch/rerank`, and get a curated product: a Jev-based rerank recipe with a
classic reranker behind it, a chat model that survives a provider outage, or the
cheapest embedding model that meets a quality bar. Meilisearch staff define and
tune these slugs without a deploy. Later, teams may define their own.

Most of the engine for this already exists in Lumen (ADR 014). What is missing is
everything that turns it into a hosted product: a staff-only way to edit it from
the Lab, a namespace, live-state routing (latency, cost), composite rerank
pipelines beyond a single remap, clear billing and BYOK rules for failover, and
the console UI.

## 2. What already exists

### 2.1 Built and merged (Lumen `main`, PR #164, ADR 014, amended by ADR 017 and 018)

| Area | State |
|---|---|
| Data model | Two layers: foundation models (`[[providers.models]]`, pure upstream bindings) and `[[virtual_models]]`, one id namespace (`[A-Za-z0-9._:/-]`), one capability per virtual model (`chat`, `embed`, `rerank`, `decisions`). |
| Storage | Dynamic config document (ADR 012): file or `config_versions` row, hot reload, ArcSwap snapshot, in-flight requests keep their plan. |
| Strategies | `single`, `fallback`, `split` (static integer weights, no stickiness), `switch` (first matching `when`). Composition by reference, depth 8, 64 flattened attempts, cycle detection. |
| Triggers | Typed `fallback_on`: `provider_error`, `rate_limited`, `timeout`, `circuit_open`, opt-in `context_length`, `content_filter`. Never after the first streamed byte. |
| Conditions | `group`, `metadata.<key>`, `has_images`, `has_tools`, `stream`, `input_tokens`, `documents`; operators `in`, `ne`, `gt(e)`, `lt(e)`, `regex`, `any`. |
| Transforms | Per-target `overrides` (allow-listed fields), chat `preset` (stored system prompt). |
| Jev rerank | A rerank target with a `remap` turns `/v1/rerank` into `decisions` calls on Jev (TypeSafe) or any decisions model (OpenAI, Perplexity, Ollama, Cloudflare): `predicate`, `score`, `composite` (1 to 8 weighted questions), `choice`; static `context`; batching; cross-capability fallback to a classic reranker (`acme/legal-rerank` example). This is what "Jev rerank" refers to. |
| Observability | `x-lumen-route`, `x-lumen-model-used`, `usage_log.route`, `lumen_virtual_model_requests_total{virtual_model, model_used}`. |
| Admin API | `GET/PUT/DELETE /admin/config/virtual_models/{id}` with `If-Match`, `GET .../{id}/plan`, dependency safety on delete. |
| Billing | Tokens and cost booked to the foundation leaf that served (for a remap, the decisions model). |
| BYOK (ADR 018) | Chain lock on the flattened attempt list: once a fallback-off team link is reached, later links with no team pool refuse (`LM-3021`), so a virtual-model chain can never silently reach platform keys. `TeamKeysExhausted` moves on like `rate_limited`. Team-served links have their own breakers. |

### 2.2 Designed but not built (Lumen `docs/backlog.md`, ADR 014 non-goals)

- Model-driven routing, "spec 2": `classify` (one Jev `choice` call over the
  targets' descriptions, `min_confidence`, default target) and `semantic`
  (embedded descriptions, `min_similarity`).
- Named, reusable remap templates (remaps are inline today).
- Presets for non-chat capabilities.
- Config history and rollback routes (`config_versions` rows exist, no API).
- Explicit non-goals of ADR 014: live-state routing (least-busy, latency or
  throughput sort, per-target RPM/TPM caps, sticky sessions), per-request routing
  in the body (OpenRouter `models`/`provider`, Portkey inline configs), per-key
  model allow-lists.

### 2.3 Gaps for a hosted product

1. **No way to edit virtual models on the hosted Gateway from the Lab.** The Lab
   proxy refuses every `/admin/config/virtual_models*` route
   (`saas/app/services/instance_allowlist.rb`, asserted in
   `saas/test/services/instance_allowlist_test.rb`), and Lumen refuses
   `/admin/config` for account-scoped calls (`403 LM-4005`). Today a virtual model
   on the hosted gateway can only be added by editing its config directly.
2. **No staff concept in the Lab.** There is no `staff`/platform-admin flag in
   `saas/` or `console/`; the `console/src/app/(lab)/admin/gateway/` area does not
   exist on `main` yet.
3. **No console UI for virtual models at all.** The Models page lists ids; only
   `UsageRow.route` is typed in `console/src/lib/lumen/types.ts`.
4. **Only global virtual models.** They live in one gateway-wide document. A team
   cannot own one, and a staff model cannot be limited to some teams (private beta).
5. **No namespace convention.** `company/model` is a convention, not a rule;
   nothing reserves a Meilisearch prefix or prevents a confusing id.
6. **No latency- or cost-aware routing.** Ordering is declared or random by weight.
7. **Composite rerank stops at one remap per target.** No cascade (cheap
   reranker narrows to top K, Jev rescores), no embedding-similarity reranker, no
   score fusion.
8. **Billing has no model dimension.** Usage events are aggregated per key and
   flush window (`operation: "gateway"`, units and `provider_cost_micro_usd`), so
   the Lab cannot price a virtual model as a product (for example a flat price
   per rerank search) without a contract change.
9. **BYOK and weighted/ranked routing are not specified together.** The chain
   lock is safe, but a `split` can pick a platform-served target while the team
   has keys for another target, so the team pays tokens it could have avoided.

## 3. What others do

| Product | How a client names it | Fallback | Load balancing / ranking | Conditional | Where it is configured |
|---|---|---|---|---|---|
| [OpenRouter](https://openrouter.ai/docs/guides/routing/model-fallbacks) | `creator/model`; variants `:nitro` (throughput), `:floor` (price), `:free`; presets `@preset/<name>` or `model@preset/<name>` ([presets](https://openrouter.ai/docs/guides/features/presets)); `openrouter/auto` | `models: [...]` array in the body, on context length, moderation, rate limit, downtime | `provider.sort: price \| throughput \| latency`, `order`, `only`/`ignore`, `max_price`, `preferred_max_latency`; default weights by price, uptime, latency, with cache stickiness ([provider routing](https://openrouter.ai/docs/features/provider-routing)) | Auto Router classifies the prompt ([model routing](https://openrouter.ai/docs/features/model-routing)) | Per request; presets in the web app, versioned |
| [Portkey](https://portkey.ai/docs/product/ai-gateway/fallbacks) | Saved config id passed as `config` / `x-portkey-config` | `strategy.mode: fallback`, `on_status_codes: [429, 503]` | `mode: loadbalance`, normalized `weight`, `sticky.hash_fields` + `ttl` ([load balancing](https://portkey.ai/docs/product/ai-gateway/load-balancing)) | `mode: conditional`, `conditions[].query` on `metadata.*`, `params.*`, `$eq/$in/$regex/$gt...`, `default` ([conditional](https://portkey.ai/docs/product/ai-gateway/conditional-routing)) | JSON configs in UI, nestable to any depth |
| [LiteLLM](https://docs.litellm.ai/docs/routing) | `model_name` = model group (alias); several deployments share it | `fallbacks`, `context_window_fallbacks`, `content_policy_fallbacks`, `order` levels | `routing_strategy`: `simple-shuffle` (rpm/tpm/weight), `least-busy`, `usage-based-routing-v2`, `latency-based-routing`, `cost-based-routing`; cooldowns `allowed_fails`, `cooldown_time` | Tag-based routing | YAML `model_list` + `router_settings` |
| [Vercel AI Gateway](https://vercel.com/docs/ai-gateway/models-and-providers/virtual-models) | `creator/model`; **virtual models** `vmc/<slug>` (team-scoped, slug immutable, archive/restore) | `models` fallback list; provider `order` (soft) vs `only` (hard) ([provider options](https://vercel.com/docs/ai-gateway/models-and-providers/provider-options)) | `sort: cost \| ttft \| tps`; session affinity for prompt cache | [Routing rules](https://vercel.com/changelog/ai-gateway-routing-rules) (rewrite / deny by source model) | Dashboard + CLI; virtual model settings win over request; works for every model type incl. rerank and decisions |
| [Cloudflare AI Gateway](https://developers.cloudflare.com/ai-gateway/features/dynamic-routing/) | `dynamic/<route>` | Rate-limit and budget-limit nodes switch to a fallback branch | Percentage node | Conditional node on body, headers, metadata | Visual flow editor, drafts, versions, deploy and instant rollback; chat completions only |
| [Martian](https://docs.withmartian.com/martian-model-router/model-router/switching-to-the-router) | `model: "router"` + `models` allow-list | Implicit | Learned router with `max_cost`, `willingness_to_pay` | Per prompt (learned) | Per request |
| [Not Diamond](https://docs.notdiamond.ai/docs/router-training-quickstart) | `select_model()` returns a model; custom routers trained on your evals | Client-side | Learned quality/cost/latency trade-off | Per prompt (learned) | API + SDK, train on up to 10k samples |
| Rerank vendors: Cohere Rerank 4, [Voyage rerank-2.5](https://openrouter.ai/voyageai/rerank-2.5), Jina reranker v3.x | Plain model id, versioned (`rerank-2.5`, `rerank-2.5-lite`, `-pro`) | n/a | n/a | n/a | Voyage and Cohere support instruction-following (a natural-language instruction in the request); Cohere bills per search unit (1 query + up to 100 docs), Voyage/Jina per token |

What to take from this:

- **A reserved prefix for saved configs is the norm** (`vmc/`, `@preset/`,
  `dynamic/`). It tells the reader a slug is a configuration, not a vendor model,
  and avoids collisions with catalog ids.
- **Saved config wins over request options** (Vercel's table is the clearest
  statement). Request-level routing in the body is a convenience the catalog
  products added later.
- **Billing is always "pay what served"** (OpenRouter states it explicitly,
  including a `:free` model falling back to a paid one). No one bills failed
  attempts. Nobody we found sells a routed slug at a flat product price; rerank
  vendors do sell rerank as a priced product.
- **Live ranking (latency, throughput, cost) is a provider-level sort** in
  OpenRouter and Vercel, backed by fleet-wide telemetry. LiteLLM does it per
  instance with in-memory windows and cooldowns. Cost sort can be static.
- **Version and rollback** is first-class in Cloudflare and OpenRouter presets.
- **Rerank as a product** is versioned and instruction-following at Voyage and
  Cohere, which is exactly the space a Jev-based rerank recipe competes in.

## 4. Recommendation

Keep ADR 014 as the engine. Do not invent a second routing system in the Lab.
Add: a namespace, ownership and lifecycle fields, one static and one live ranking
mode, a rerank `cascade`, a staff editor in the Lab, and explicit billing and BYOK
rules. Lumen stays the only place where a routing tree is validated and executed;
the Lab is an editor and a catalog view, never a second copy of the tree.

### 4.1 Data model

Extend the existing `[[virtual_models]]` entry (all new fields optional, so
existing configs keep loading):

```toml
[[virtual_models]]
id = "meilisearch/rerank"              # reserved prefix, see 4.2
capability = "rerank"
strategy = "fallback"
display_name = "Meilisearch Rerank"     # new: UI label
description = "Jev relevance with Cohere as a backup"
status = "active"                       # new: active | archived (archived answers LM-2001, keeps config)
listed = true
groups = []                             # new: empty = every group; else an allow-list (private beta)
rank = "declared"                       # new on fallback: declared | cost | latency (4.3)
billing = "passthrough"                 # new, reserved: passthrough only in v1 (4.4)

[[virtual_models.targets]]
model = "typesafe/jev"
remap = { strategy = "predicate", context = "Rank documents for a search engine result page." }

[[virtual_models.targets]]
model = "cohere/rerank-v3.5"
```

Ownership, in two phases:

- **Phase 1, platform virtual models (staff only).** Stay in the gateway-wide
  config document, so they keep hot reload, `If-Match`, `/plan` and
  `config_versions`. Editable only by the Lab as platform (master key, no
  `X-Lumen-Account-Ref`).
- **Phase 2, team virtual models (if we want customer-defined ones).** A new
  store keyed by budget group, like the BYOK pools of ADR 018:
  `group_virtual_models(group_id, slug, document)`, admin routes
  `/admin/groups/{id}/virtual_models[/{slug}]`, account-scoped by
  `X-Lumen-Account-Ref`. A team model may target foundation models and platform
  virtual models but not another team's models; it compiles into a per-group
  overlay of the plan, resolved only when the caller's group matches. Same
  validation, same limits, and a per-group cap (for example 50 models).

`groups` is a cheap answer to "only some teams can see this" without phase 2:
the resolver checks the caller's group before decide, and `GET /v1/models` hides
the entry from other groups. A disallowed caller gets `LM-2001`, as if the id did
not exist.

### 4.2 Slug naming

| Kind | Form | Example |
|---|---|---|
| Foundation model | `<provider>/<model>` | `openai/gpt-4o`, `typesafe/jev`, `cohere/rerank-v3.5` |
| Platform virtual model, stable | `meilisearch/<product>` | `meilisearch/rerank`, `meilisearch/chat`, `meilisearch/embed-multilingual` |
| Platform virtual model, pinned | `meilisearch/<product>-<yyyy-mm>` | `meilisearch/rerank-2026-11` |
| Team virtual model (phase 2) | `team/<slug>` | `team/support-bot` |

- `meilisearch/` and `team/` become **reserved prefixes**: a foundation model or a
  platform model cannot use `team/`, and nothing but a platform virtual model can
  use `meilisearch/`. Validation error otherwise.
- **Stable and pinned.** The stable slug is a `single` virtual model pointing at
  the current pinned one. Staff move the stable slug forward when a new recipe is
  ready; customers who need reproducible scores pin the dated slug. This copies
  the vendor model-version practice (`rerank-2.5`, `-latest`) without new
  machinery: it is two virtual models.
- `team/<slug>` resolves in the caller's group, like Vercel's `vmc/<slug>`, so two
  teams can both have `team/support-bot` and neither sees the other's. The slug is
  immutable after creation (rename = new model + archive), as at Vercel.
- No OpenRouter-style suffix variants (`:floor`, `:nitro`) in v1. If wanted later,
  `meilisearch/chat:cheap` could be sugar for a `rank = "cost"` sibling, but each
  suffix is a new public contract.

### 4.3 Strategies, v1 versus later

**v1 (smallest set that makes the hosted product credible)**

1. Everything already built: `single`, `fallback`, `split`, `switch`, `remap`,
   `overrides`, `preset`.
2. `rank = "cost"` on `fallback`: order the targets at load by the foundation
   model's list price (chat: `cost_per_1m_input + cost_per_1m_output` weighted by a
   configurable `output_ratio`, default 0.25; embed and rerank: their single unit
   price; a remap target: the decisions model's input price times the expected
   question count). Static, zero request-path cost, shows in `/plan`. Ties keep
   declaration order. This is the LiteLLM `cost-based-routing` and Vercel
   `sort: "cost"` behaviour without live state.
3. Rerank `cascade` (new strategy, `rerank` only): an ordered list of stages, each
   a rerank target with `top_k`. Stage 1 scores all documents, the top `top_k`
   go to stage 2, and so on; the final scores come from the last stage, documents
   cut earlier keep their order below. Typical recipe: `cohere/rerank-v3.5` with
   `top_k = 50`, then Jev `composite`. It is a pipeline, not a fallback: every
   stage runs. A failing stage falls back as a whole only if the cascade is
   itself a target of a `fallback`.
4. Platform namespace, `display_name`, `status`, `groups` (4.1, 4.2).

**Later**

| Feature | Notes |
|---|---|
| `rank = "latency"` | Per-instance EWMA of time to first byte (chat) or total latency (embed, rerank) per (provider, model) leaf, updated with atomics on each served attempt, decayed, with a minimum sample count before it reorders. Reorders `fallback` targets in decide; ties within `latency_buffer` (default 10%) keep declared order, LiteLLM-style, so traffic does not flap. Per instance only; good enough with few gateway replicas. |
| `least_busy` on `split` | In-flight counters per leaf; the decide step reads them. |
| Per-target RPM/TPM caps | A target over its cap is skipped like an open circuit (Cloudflare rate-limit node, LiteLLM usage-based v2). |
| Sticky `split` | Hash of `metadata.<key>` or the virtual key id picks the bucket, so a conversation keeps its prompt cache (Portkey `sticky`, Vercel session affinity). |
| `classify` / `semantic` | Backlog spec 2; Jev `choice` as the router. |
| Embedding-similarity reranker | A `remap = { strategy = "embed_similarity" }` onto an embed model: cosine between query and document vectors. Cheap first stage of a cascade. |
| Score fusion | `fuse` over several rerankers (weighted mean or reciprocal rank fusion). |
| Request-level overrides | OpenRouter-style `models: [...]` in the body. Probably never on the hosted gateway: the catalog is the product. |
| History and rollback | The backlog's `GET /admin/config/history` + `POST .../rollback/{id}`, scoped to the virtual-model section. |

### 4.4 Billing and usage events with failover

Rules (v1, no contract change):

1. **Pay for what served.** Tokens and provider cost are booked to the foundation
   leaf that served, as today. A fallback to a more expensive model costs the
   customer more; that is the industry norm (OpenRouter documents it).
2. **Failed attempts are absorbed by Meilisearch.** An attempt that fails before
   committing (5xx, timeout, 429, open circuit) is never billed to the customer,
   even if the provider charged us (for example a timeout after the provider
   started generating). This needs to be checked in code and stated in ADR 019:
   the attempt's partial usage must not reach `settle`.
3. **Pipelines bill every stage.** A `cascade` is one request whose cost is the
   sum of its stages; `usage.total_tokens` and `provider_cost_micro_usd` add up.
   A remap's cost stays the decisions model's input tokens.
4. **Bandwidth is per client request**, once, whatever the number of attempts
   or stages (it is metered at the HTTP edge, ADR 018).
5. **A router call (later `classify`/`semantic`) is billed** as its own model's
   cost on the same request, recorded separately in the usage log.

Product pricing (open question 1): selling `meilisearch/rerank` at a flat price
per search makes the bill predictable and independent of the branch that served.
It needs a model dimension in usage events: today an event is one aggregate per
key per flush window. The change would be one event per (key, billing line)
with `units.searches` and a `sku` (`meilisearch/rerank`), and the Lab pricing it
from `pricing.yml` instead of marking up `provider_cost_micro_usd`. That is a
platform contract change (Lab-owned `lab-events.schema.json` plus Lumen's vendored
copy, same task). `billing = "passthrough"` is reserved now so the field exists
when this lands.

### 4.5 BYOK and "never fall back to Meilisearch keys"

The ADR 018 chain lock already applies to virtual models, because it runs on the
flattened attempt list. What this design adds:

1. **The lock is about failover, not pipelines.** In a `cascade`, every stage
   runs. A stage on a provider the team has no key for is served on platform keys
   and billed tokens, which is the documented ADR 018 behaviour for providers
   without a team pool. The fallback-off lock only governs moving to a *later
   attempt* after a team link fails. This must be written down, because a team
   could read "fallback off" as "never use platform keys".
2. **Prefer team-served targets in `split` and ranked `fallback`.** When the
   caller's group has a usable pool for some targets, decide puts those targets
   first (a stable partition after the weight pick or the rank), then applies the
   chain lock. A team that brought an OpenAI key and calls a 50/50 OpenAI/Anthropic
   split is then served on its own key, not billed tokens half the time.
   Declared-order `fallback` and `switch` are left alone: their order is a
   deliberate staff choice (open question 3).
3. **Ranking uses list prices, not the caller's cost.** `rank = "cost"` is computed
   at load from list prices, so it is the same for every team; the BYOK
   preference of point 2 is what moves team-served targets up.
4. **`LM-3021` stays the answer** when the chain lock stops a chain, and remains
   unbilled (no upstream call). The `x-lumen-route` header shows how far it got.
5. **Jev rerank and BYOK.** A remap onto `typesafe/jev` uses the team's TypeSafe
   pool when it has one (TypeSafe is a plain-key kind). A Jev pool that is
   exhausted with fallback off moves on to the Cohere target only if the team also
   has a Cohere pool; otherwise `LM-3021`.

### 4.6 Lab: staff admin UI and team-facing views

**Proxy and auth (saas/).**

- Add a staff flag (`users.staff` boolean, set by console or seed; no self-serve).
- New proxy rules for the built-in hosted instance only, role `staff`:
  `GET /admin/config/virtual_models`, `GET|PUT|DELETE /admin/config/virtual_models/:id`,
  `GET /admin/config/virtual_models/:id/plan`, and read-only
  `GET /admin/config/providers` for the target picker. Never forwarded with
  `X-Lumen-Account-Ref`. Every write is logged in a Lab audit table (who, slug,
  before and after hash).
- Lumen gains `PUT ...?dry_run=true` (validate, return the compiled `/plan`,
  persist nothing) so the editor can show errors before saving.

**Staff area: `console/src/app/(lab)/admin/gateway/virtual-models/`.**

- **List.** Slug, display name, capability, strategy, status, groups (all or N),
  requests and error rate over 24 h and top `model_used` (from
  `lumen_virtual_model_requests_total` and the usage export grouped by `route`).
  Filters by capability and status. Archive and restore from the row menu.
- **Editor** (react-hook-form + zod schema mirroring Lumen's validation, Lumen
  stays authoritative):
  - Header: slug (immutable after creation, `meilisearch/` prefix fixed),
    display name, description, capability, status, listed, groups picker.
  - Strategy picker, then a targets table: model picker filtered by capability
    (foundation and virtual), drag to reorder for `fallback`, weight inputs with a
    live percentage for `split`, a rule builder for `switch` (attribute, operator,
    value, AND rows, OR groups), stages with `top_k` for `cascade`, `fallback_on`
    checkboxes, `rank` select.
  - Remap panel on rerank targets that point at a decisions model: strategy,
    context, and a list editor for `composite` questions with weights.
  - Overrides and preset panels behind "Advanced".
  - Save sends `If-Match`; a `412` shows "changed since you opened it" with a
    reload-and-diff action.
- **Plan tab.** Renders `/plan` as a tree (strategy, weights, conditions,
  triggers down to foundation leaves), with each leaf's price and, later, latency.
- **Try tab.** Sends a request through the console playground key and shows the
  response, `x-lumen-route`, `x-lumen-model-used`, latency and cost. For rerank,
  a query plus documents form with the score table side by side with a plain
  reranker, so staff can compare a Jev recipe with Cohere before publishing.
- **History tab** once the rollback routes exist.

**Team-facing console (`console/src/app/(lab)/gateway/[slug]/`).**

- Models page: virtual models listed with a "Meilisearch" badge, display name,
  description and capability; the routing tree stays private (customers see what
  served on each request via `x-lumen-model-used`, not the recipe).
- Rerank and chat playground pages: `meilisearch/*` slugs first in the picker.
- Usage page: a "Served by" breakdown from `usage_log.route` / `model_used`.
- Phase 2 only: a team "Virtual models" page reusing the editor components,
  gated by `useLabRole().can("admin")`, slug prefix `team/`.

Co-location: the editor components live under the admin route; the team page in
phase 2 would make them shared by two features, still under the threshold for a
shared `components/` move.

### 4.7 Example catalog for launch

```toml
# Stable slug -> current pinned recipe.
[[virtual_models]]
id = "meilisearch/rerank"
capability = "rerank"
strategy = "single"
display_name = "Meilisearch Rerank"
targets = [{ model = "meilisearch/rerank-2026-11" }]

# Pinned recipe: Jev relevance, Cohere when Jev is overloaded or down.
[[virtual_models]]
id = "meilisearch/rerank-2026-11"
capability = "rerank"
strategy = "fallback"
listed = false
[[virtual_models.targets]]
model = "typesafe/jev"
remap = { strategy = "predicate", context = "Rank documents for a search results page." }
[[virtual_models.targets]]
model = "cohere/rerank-v3.5"

# Cascade: Cohere narrows to 50, Jev composite rescores (new strategy).
[[virtual_models]]
id = "meilisearch/rerank-pro"
capability = "rerank"
strategy = "cascade"
[[virtual_models.targets]]
model = "cohere/rerank-v3.5"
top_k = 50
[[virtual_models.targets]]
model = "meilisearch/jev-composite"     # a single-target remap model

# Cheapest chat model that survives an outage (rank = "cost").
[[virtual_models]]
id = "meilisearch/chat-fast"
capability = "chat"
strategy = "fallback"
rank = "cost"
targets = [{ model = "openai/gpt-4o-mini" }, { model = "anthropic/claude-haiku-4-5" }, { model = "mistral/mistral-small" }]
```

## 5. Delivery plan (sketch)

1. Lumen: ADR 019 (namespace, `display_name`, `status`, `groups`, `rank = "cost"`,
   `cascade`, BYOK preference, failed-attempt billing rule), `dry_run` on PUT.
2. Lab: staff flag, proxy rules and audit log; re-vendor the Lumen OpenAPI.
3. Console: admin list, editor, plan and try tabs; team-facing badge and usage
   breakdown.
4. Later: `rank = "latency"`, history and rollback, product pricing (contract
   change), team virtual models (phase 2), `classify`/`semantic`.

## 6. Open questions for Quentin

1. **Pricing.** Pass-through cost of whatever served (v1 default), or a flat
   product price per `meilisearch/*` slug (per search for rerank)? The second
   needs a usage-event contract change.
2. **Customer-defined virtual models.** Do teams get `team/<slug>` (phase 2), or
   is the catalog staff-only for now? If yes, may they reference platform keys
   at all, or only their BYOK providers?
3. **BYOK preference.** Should team-served targets also jump ahead in a
   declared-order `fallback` (stronger "your keys first" promise), or only in
   `split` and ranked fallbacks as proposed?
4. **Recipe visibility.** Do customers see the routing tree of a `meilisearch/*`
   model (transparent, like Vercel's settings) or only the model that served
   (keeps the Jev recipe as Meilisearch IP)?
5. **Instruction-following rerank.** Voyage and Cohere accept a per-request
   instruction. Should `meilisearch/rerank` accept a client `instructions` field
   that goes into the Jev state? ADR 014 says remap text is never client-supplied;
   a client field would be a separate, size-capped, logged-as-length-only input.
6. **Pinning policy.** Dated pinned slugs (`-2026-11`) with a deprecation window,
   or only stable slugs and staff-managed changes?
7. **Latency routing scope.** Is per-instance latency good enough, or does the
   hosted gateway run enough replicas that we need shared state (which ADR 014
   avoided on purpose)?
8. **Staff identity.** A `users.staff` flag in the Lab, or reuse an existing
   Meilisearch SSO group?
9. **Failed attempt cost.** Confirm that Meilisearch absorbs the provider cost of
   failed attempts (timeouts after generation started can be billed by the
   provider).
