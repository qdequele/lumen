# Virtual models

A **virtual model** is a public id, such as `acme/legal-rerank`, that carries
routing logic: ordered fallback, weighted split, conditional routing, request
templates, and Jev answering `/v1/rerank`. It sits on top of the **foundation
models** you declare under `[[providers.models]]`, which stay plain upstream
bindings. The design is recorded in [ADR 014](adr/014-virtual-models.md).

The examples below assume the foundation models of
[`config.example.toml`](https://github.com/qdequele/lumen/blob/main/config.example.toml)
(`openai/gpt-4o`, `claude-sonnet-4-5`, `jev`, `cohere/rerank-english`, and so
on). Every snippet is valid TOML; note that a TOML inline table `{ ... }` must
stay on one line, so long targets use `[[virtual_models.targets]]` sub-tables.

## 1. Foundation models vs virtual models

| | Foundation model | Virtual model |
|---|---|---|
| Declared in | `[[providers.models]]` | `[[virtual_models]]` |
| What it is | An upstream binding: `id`, `upstream_id`, `capabilities`, `modalities`, prices, `release_date` | A name with logic over one or more targets |
| Callable by clients | Yes | Yes |
| Can carry routing logic | No | Yes |

Both layers share **one id namespace**. Ids are non-empty and use only the
characters `A-Z a-z 0-9 . _ : / -`; `company/model` is a convention, not a
rule. A collision between a foundation id and a virtual id is a validation
error naming both. A virtual model serves exactly **one capability** (`chat`,
`embed`, `rerank` or `systemone`); to offer the same product name for two
capabilities, define two ids.

Virtual models live in the dynamic config document ([config
modes](operations/config-modes.md)): they hot-reload and are editable through
the admin API (section 9). An in-flight request keeps the plan it started
with.

`GET /v1/models` lists virtual models next to foundation models, with
`"virtual": true`, a single-entry `capabilities` array, the optional
`description`, and the modalities common to every foundation model the virtual
model can reach. Foundation entries carry `"virtual": false`. Set
`listed = false` to hide a virtual model from the list; it stays callable.

```toml
[[virtual_models]]
id = "gpt-4o"
capability = "chat"
strategy = "fallback"
description = "GPT-4o with a Claude backup"
listed = true
targets = [{ model = "openai/gpt-4o" }, { model = "claude-sonnet-4-5" }]
```

Calling an id that is neither kind is `LM-2001`; calling a virtual model on
the wrong endpoint (say a rerank model on `/v1/chat/completions`) is
`LM-2002`.

## 2. Strategies

Every virtual model has one `strategy`:

| Strategy | Behaviour | Targets |
|---|---|---|
| `single` | The one target. | Exactly 1 |
| `fallback` | Targets in order; move to the next when the failure matches `fallback_on`. | 2 or more |
| `split` | Pick one target at random by integer `weight` (> 0, required on every target). If it fails with a matching trigger, try the others in descending weight order (ties keep declaration order). No stickiness. | 2 or more |
| `switch` | The first target whose `when` matches. A chosen branch never falls through to the next rule; its resilience comes from what it points at (for example a `fallback` virtual model). | Every target but the last has a `when`; the last has none and is the mandatory default |

Field placement is enforced: `weight` only under `split`, `when` only under
`switch`, `fallback_on` only on `fallback` and `split`, `preset` only on
`chat`.

```toml
# 80% of traffic to OpenAI, 20% to Azure OpenAI; a failing pick moves to the other.
[[virtual_models]]
id = "acme/chat-ab"
capability = "chat"
strategy = "split"
targets = [{ model = "openai/gpt-4o", weight = 80 }, { model = "azure-gpt-4o", weight = 20 }]
```

### Composition and limits

A target's `model` may be another virtual model, so a fallback can point at a
split, a switch branch at a fallback, and so on. At load (boot, reload, and
every admin write) the gateway checks the following and reports the first
failure with the virtual model id and the reason:

1. every `model` reference exists;
2. strategy shape (the table above);
3. every target serves the virtual model's capability, or carries a `remap`
   that makes it compatible (section 7);
4. no cycles (the error prints the path, for example `a -> b -> a`);
5. nesting depth at most **8** virtual models from the requested id down to a
   foundation model;
6. at most **64** attempts per request: a `fallback` or `split` counts the
   attempts of every target, a `switch` only its largest branch, and a
   foundation target counts 1 (a DAG that lists the same child several times
   per level multiplies quickly; the error names the model and its count);
7. the contents of `overrides`, `preset` and `remap`.

A `preset` on a virtual model that is only reached through another one is
ignored at request time (section 6), and loading logs a warning.

## 3. `fallback_on` triggers

`fallback_on` lists the failures that let a `fallback` or `split` move on to
its next target.

| Trigger | Matches |
|---|---|
| `provider_error` | Retryable upstream 5xx (`LM-3003`, including a TypeSafe `529 Overloaded`), an unreachable upstream, or a stream that ends before its first content frame (`LM-3010`). A malformed response (`LM-3002`) and non-retryable upstream statuses never trigger a fallback. |
| `rate_limited` | Upstream 429 (`LM-3001`). |
| `timeout` | Connect, first-token and per-attempt timeouts (`LM-3005`, `LM-3011`, `LM-3012`). |
| `circuit_open` | The target's circuit breaker is open. |
| `context_length` | Opt-in. The upstream rejected the input as too long (surfaces as `LM-2012` when nothing absorbs it). |
| `content_filter` | Opt-in. The upstream refused on content policy (surfaces as `LM-2013`). |

- **Default** when `fallback_on` is omitted:
  `["provider_error", "rate_limited", "timeout", "circuit_open"]`, exactly the
  behaviour of the earlier per-model `fallbacks`.
- `context_length` and `content_filter` rely on the provider classifying its
  upstream error body. A provider that cannot classify an error leaves it as a
  plain client error, and nothing fails over. Classification is implemented
  for OpenAI and the OpenAI-compatible hosts, Anthropic, Google/Vertex,
  Bedrock, Mistral and Cohere.
- **Never triggers:** a malformed upstream response (`LM-3002`), a
  non-retryable upstream status (for example a TypeSafe `401` or `422`, which
  surface as `LM-3003` but are not retried), any other client 4xx, a client
  cancellation (which never touches a circuit breaker, [ADR 006](adr/006-client-cancellation-error-code.md)),
  and any failure after the first streamed byte. Once a streaming response has
  delivered content the request is committed; a later error becomes an SSE
  error frame, never a fail-over ([Streaming](chat/streaming.md)).
- Retries still happen inside each target first
  ([Resilience tuning](operations/resilience.md)), and
  `[resilience].total_timeout_ms` bounds the whole tree (`LM-3013`).

```toml
# Also fail over when the prompt is too long for the first model.
[[virtual_models]]
id = "acme/long-context"
capability = "chat"
strategy = "fallback"
fallback_on = ["provider_error", "timeout", "context_length"]
targets = [{ model = "openai/gpt-4o" }, { model = "gemini-long" }]
```

## 4. `switch` conditions

A `switch` target carries a `when` table. Attributes:

| Attribute | Type | Source |
|---|---|---|
| `group` | string | The authenticated key's budget group ([keys and budgets](operations/keys-budgets.md)). Missing when auth is off or the key has no group. |
| `"metadata.<key>"` | string, number or bool | A top-level field of the `x-lumen-metadata` header (alias `cf-aig-metadata`, [ADR 002](adr/002-request-metadata-header.md)). |
| `has_images` | bool | A chat or embed request contains an image part. |
| `has_tools` | bool | The chat request declares tools. |
| `stream` | bool | The chat request is streaming. |
| `input_tokens` | integer | The [token accounting](operations/token-accounting.md) input estimate. Computed only when some rule in the plan references it. For a chat preset, the preset's system prompt is applied before routing, so it counts. |
| `documents` | integer | The rerank document count. |

The virtual key id is deliberately not an attribute. Using an attribute the
capability never has (for example `has_tools` on a rerank model) is a
validation error.

**Quote `metadata.<key>` in TOML.** An unquoted `metadata.plan = "pro"` is a
nested table `metadata = { plan = "pro" }` and is rejected; write
`"metadata.plan" = "pro"`.

**Operators.** A bare value means equality. Otherwise use a one-key table:
`in` (a list), `ne`, `gt`, `gte`, `lt`, `lte`, or `regex` (compiled at load;
an invalid regex is a validation error). Several keys in one `when` table are
ANDed. `any = [ {...}, {...} ]` is an OR of tables and can nest. A missing
attribute makes its comparison false (including `ne`), so the branch does not
match and evaluation moves on to the next rule.

```toml
[[virtual_models]]
id = "acme/chat"
capability = "chat"
strategy = "switch"
targets = [
  { when = { group = "eu-customers" }, model = "acme/chat-eu" },
  { when = { "metadata.plan" = { in = ["pro"] }, input_tokens = { gt = 32000 } }, model = "gemini-long" },
  { when = { any = [{ has_images = true }, { has_tools = true }] }, model = "gpt-4o" },
  { model = "local-llama" },
]
```

The first rule sends a whole budget group to a regional model. The second
needs both the metadata plan and a long input. The third sends image or tool
requests to a capable model. The last target has no `when` and is the default.
The regex operator works the same way: `{ group = { regex = "^tenant-[0-9]+$" } }`.

## 5. Overrides

`overrides` on a target rewrites request fields for that target:

```toml
targets = [{ model = "openai/gpt-4o", overrides = { set = { max_tokens = 4096 }, default = { temperature = 0.3 }, drop = ["seed"] } }]
```

- `set` always applies; `default` applies only when the client did not send the
  field; `drop` removes it.
- Overrides act on the parsed request, not on JSON paths, and each capability
  has an allow-list of fields:

| Capability | Allowed fields |
|---|---|
| `chat` | `temperature`, `top_p`, `max_tokens`, `max_completion_tokens`, `stop`, `seed`, `presence_penalty`, `frequency_penalty`, `response_format`, `reasoning_effort`, `parallel_tool_calls` |
| `embed` | `dimensions`, `encoding_format` |
| `rerank` | `top_n` |
| `systemone` | none |

An unknown field, a field outside the allow-list, or a value of the wrong type
is a validation error.

**Order of application** for one request: the requested virtual model's
`preset` (section 6) first, then target overrides from the outermost virtual
model inward, so the level closest to the foundation model wins.

## 6. Presets (chat)

A `preset` gives a chat virtual model a stored system prompt and its own
sampling defaults:

```toml
[[virtual_models]]
id = "acme/support-bot"
capability = "chat"
strategy = "fallback"
targets = [{ model = "gpt-4o" }, { model = "claude-sonnet-4-5" }]
description = "Acme support assistant"
preset = { system_prompt = "You are Acme's support assistant. Answer briefly and point to the documentation when relevant.", system_prompt_mode = "prepend", overrides = { default = { temperature = 0.2 }, set = { max_tokens = 800 } } }
```

| `system_prompt_mode` | Effect |
|---|---|
| `prepend` (default) | The preset becomes the first message, before any client system message. |
| `replace` | Client system messages are removed; the preset takes their place. |
| `if_absent` | Applied only when the client sent no system message. |

- `system_prompt` is at most **32 KiB** and must not be blank. `overrides` uses
  the syntax and allow-list of section 5.
- **Token accounting.** The injected prompt is part of the request: it counts
  in `usage.prompt_tokens`, in the usage log and in cost, exactly as if the
  client had sent it. It is applied before routing, so `input_tokens` switch
  conditions count it too.
- **Only on the requested id.** A preset applies only when its own virtual
  model is the id the client sent. A preset on a virtual model reached through
  composition is ignored, and validation warns about it at load.
- Preset text is operator-authored config: never client-supplied, never
  logged, never echoed in errors.

## 7. Jev as a reranker

A `remap` on a target of a **rerank** virtual model converts `POST
/v1/rerank` into [SystemOne](systemone/systemone.md) calls. The target must
point **directly** at a foundation model that serves `systemone` (a
`kind = "typesafe"` model such as `jev`). A `typesafe` model can no longer
declare `rerank` itself.

The state sent upstream is `{ "query": <query> }`, plus `"context"` when set.
Each question's instructions are structured as `{ "document": <document>,
"question": <instructions> }`.

| `strategy` | Fields | Questions per request | `relevance_score` |
|---|---|---|---|
| `noul` (default) | `instructions`, `criteria.true`, `criteria.false` (all default to a generic relevance question) | One noul per document | The noul |
| `score` | `instructions`, `levels` (worst first, 2 to 10 entries) | One score per document | `level / (levels - 1)` |
| `composite` | `questions`: 1 to 8 entries of `{ instructions, criteria.true, criteria.false, weight > 0 }` | N noul per document | `sum(weight_i * noul_i) / sum(weight_i)` |
| `choice` | `instructions` | One choice question; the options are the documents | The option's probability (scores sum to 1) |

`context` is a static, operator-authored string sent in the state next to the
query, for rules that hold for every request of this virtual model ("US
federal case law. Prefer holdings over dicta.").

**Batching and limits**

- `noul`, `score` and `composite` are batched: at most 100 documents and about
  48k estimated tokens per upstream call, with 4 calls in flight. For
  `composite` the per-call budget counts every question of every document.
- `choice` must fit **one call**, because its scores are relative and cannot be
  merged across calls: at most **255 documents** and within the per-call token
  budget. Otherwise the request fails with `LM-1001` (400) naming the limit,
  before any upstream call.
- Documents are truncated to about 4,096 estimated tokens.

**Accounting.** `usage.total_tokens` is the sum of the `input_tokens` Jev
reports over all calls (upstream-reported, not `estimated`). Cost is the
SystemOne model's `cost_per_1m_input`; `search_units` stay derived from the
document count. `x-lumen-model-used` names the SystemOne model.

**Jev with a Cohere fallback.** A remap target and a plain reranker in one
`fallback` give a cross-capability fallback. A TypeSafe `529 Overloaded` is a
`provider_error`, so the default triggers already send the request to Cohere.
This composite example is also in `config.example.toml`:

```toml
[[virtual_models]]
id = "acme/legal-rerank"
capability = "rerank"
strategy = "fallback"

[[virtual_models.targets]]
model = "jev"
[virtual_models.targets.remap]
strategy = "composite"
context = "US federal case law. Prefer holdings over dicta."
[[virtual_models.targets.remap.questions]]
instructions = "Is `document` on the topic of the query?"
weight = 0.3
criteria.true = "It discusses the same legal question."
criteria.false = "It is about something else."
[[virtual_models.targets.remap.questions]]
instructions = "Does `document` state the rule the query cites?"
weight = 0.7
criteria.true = "It states the specific rule."
criteria.false = "It only mentions related topics."

[[virtual_models.targets]]
model = "cohere/rerank-english"
```

Per-tenant relevance rules combine `remap` with `switch` on the budget group:

```toml
[[virtual_models]]
id = "acme/rerank"
capability = "rerank"
strategy = "switch"
targets = [{ when = { group = "law-firm" }, model = "acme/legal-rerank" }, { model = "jev-rerank" }]
```

The other strategies in one line each:

```toml
[[virtual_models]]
id = "acme/graded-rerank"
capability = "rerank"
strategy = "single"

[[virtual_models.targets]]
model = "jev"
remap = { strategy = "score", instructions = "How well does `document` answer the query?", levels = ["off topic", "related", "partial answer", "full answer"] }

[[virtual_models]]
id = "acme/pick-rerank"
capability = "rerank"
strategy = "single"

[[virtual_models.targets]]
model = "jev"
remap = { strategy = "choice", instructions = "Which document best answers the query?" }
```

`score` with 4 levels gives `0`, `1/3`, `2/3` and `1`. Remap text is
operator-authored config: never client-supplied, never logged, never echoed in
errors.

## 8. Observability

| Where | What |
|---|---|
| `x-lumen-model-used` response header | The foundation model that served (unchanged). |
| `x-lumen-route` response header | The path taken, for example `acme/chat>acme/chat-eu>mistral-large`. Set when a virtual model was requested. |
| `usage_log.model` / `model_used` | The requested id / the foundation model that served. |
| `usage_log.route` | The same path as the header (nullable; empty for a direct foundation call). See the [usage log](operations/usage-log.md). |
| `lumen_virtual_model_requests_total{virtual_model, model_used}` | Counter of requests served through a virtual model. Virtual ids are operator-defined, so cardinality is bounded. See [metrics](operations/metrics.md). |
| Tracing | `virtual_model` and `route` span fields. Never prompts, presets or remap text. |

Tokens and cost are booked to the foundation model that served, so a fallback
that fires costs what the fallback model costs ([token
accounting](operations/token-accounting.md)).

## 9. Admin API

All routes are under the config admin surface ([config
modes](operations/config-modes.md)) and work in both `config_source` modes.

| Route | Purpose |
|---|---|
| `GET /admin/config/virtual_models` | List (`id`, `capability`, `strategy`) plus the document `hash`. |
| `GET /admin/config/virtual_models/{id}` | One virtual model plus `hash`. An unknown id is `404` `LM-1003`. |
| `PUT /admin/config/virtual_models/{id}` | Create or replace. Requires `If-Match` with the current hash, like every mutating config route. A path id that differs from the body `id` is `LM-1001`. |
| `DELETE /admin/config/virtual_models/{id}` | Remove. Requires `If-Match`. |
| `GET /admin/config/virtual_models/{id}/plan` | The fully resolved tree: every reference expanded down to foundation models, with strategies, weights, conditions and triggers. |

`{id}` is percent-encoded when it contains `/`:

```bash
curl -s -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  http://localhost:8080/admin/config/virtual_models/acme%2Flegal-rerank/plan
```

The `/plan` body shows preset and remap text, since it is an admin route
behind the master key; it is never logged.

**Dependency safety.** Deleting a provider, a foundation model or a virtual
model that another virtual model still references is `LM-1001`, naming the
dependent. Every write is validated with the same rules as boot, so an invalid
virtual model (a cycle, a missing reference, a bad `when`) is `LM-1001` naming
the model and the reason.

## 10. Migrating from `fallbacks`

Per-model `fallbacks`, the `[providers.models.rerank]` block, and the `rerank`
capability on `typesafe` models are removed. A config that still uses one
fails validation naming the model, and prints the converted snippet.

```bash
lumen config migrate -c config.toml --dry-run   # print the result, change nothing
lumen config migrate -c config.toml             # rewrite (file mode keeps config.toml.bak)
```

`lumen config migrate [-c PATH] [--dry-run]` rewrites the document. In file
mode it backs the file up to `<file>.bak` and replaces it atomically; in DB
mode it writes a new `config_versions` row. Formatting and comments outside
the edited tables are kept, and the result is validated before anything is
written.

**The rename rule.** To keep clients working unchanged, the *virtual* model
takes over the public id and the foundation model is renamed:

- A foundation model `M` on provider `P` with `fallbacks = [F1, F2]` becomes
  the foundation model `P/M`, and a virtual `M` is created with
  `strategy = "fallback"` and `targets = [P/M, F1, F2]`. The renamed model
  keeps sending its old upstream id: the migrator pins `upstream_id` to the
  old value when the model had none.
- Only models with `fallbacks` are renamed. If `P/M` already exists, the
  migrator stops with an error naming it.
- If `M` declares several capabilities, one id can carry only one virtual
  model: the virtual `M` gets the first declared capability, and the others
  are printed as needing a decision. Until you add a second virtual model, a
  call to `M` for another capability returns `LM-2002`; it still works through
  `P/M`, without fallback. `--dry-run` shows this before anything is written.
- A `typesafe` model `R` declaring `rerank` (with or without a converter
  block) becomes a virtual `R` with `capability = "rerank"`,
  `strategy = "single"`, and one target with a `noul` remap that copies the
  converter's fields, pointing at the provider's SystemOne foundation model
  with the same `upstream_id`. If none exists, the migrator creates
  `P/<upstream_id>` with `capabilities = ["systemone"]`. When the prices of the
  removed reranker differ from that model's, a note asks you to review them.

**Visible side effect.** `usage_log.model_used` and the Prometheus model
labels of renamed foundation models change, for example `gpt-4o` becomes
`openai/gpt-4o`. The id clients send does not change. Update dashboards and
queries that filter on the old value.

Before and after:

```toml
# before (no longer valid)
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["claude-sonnet-4-5"]
```

```toml
# after
[[providers.models]]
id = "openai/gpt-4o"
upstream_id = "gpt-4o"
capabilities = ["chat"]

[[virtual_models]]
id = "gpt-4o"
capability = "chat"
strategy = "fallback"
targets = [{ model = "openai/gpt-4o" }, { model = "claude-sonnet-4-5" }]
```

See also [ADR 014](adr/014-virtual-models.md) and the [Error
codes](errors.md).
