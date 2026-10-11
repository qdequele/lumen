# Decisions (typed answers)

`POST /v1/decisions` answers typed questions about some content with
calibrated probabilities, instead of generating text. A decision model gets an
input (text, or text and images) and a list of questions, and returns one
answer per question: a probability for a yes/no question, a distribution over
options for a choice, a distribution over ordered levels for a score.

Decision models come from several vendors: TypeSafe (Jev), Perplexity
(`pplx-decider-*`), OpenAI (`gpt-6-luna`), Cloudflare (Clef), local models
through Ollama (Nimble, Clef, Kev) and any vendor that speaks the TypeSafe
format (Liquid, Inception, Upstage). LUMEN serves all of them behind one
capability, `decisions`, so any of them can be a target of the same
[virtual model](../virtual-models.md) (fallback, split, switch) and a
[reranker](../reranking/reranking.md#decision-models-as-rerankers-jev-perplexity-openai-ollama-cloudflare).
The design and its trade-offs are in
[ADR 017](../adr/017-decisions-capability.md).

The `model` field is one of *your* configured model ids: the `id` of a
`[[providers.models]]` block declaring `capabilities = ["decisions"]`, or a
virtual model of capability `decisions`. Provider setup is in
[Providers](../providers.md#decisions-by-provider-kind).

## Configuration

```toml
[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"

[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["decisions"]
cost_per_1m_input = 0.042
```

The old capability spelling `systemone` still loads (with a boot warning);
`lumen config migrate` rewrites it.

## Two request formats

`/v1/decisions` accepts both formats, detects which one from the body, and
answers in the format it received. Clients of every vendor point at LUMEN with
no code change: the OpenAI SDK, Perplexity-style and TypeSafe-format HTTP
clients (TypeSafe clients that post to `/v1/systemone` switch the path to
`/v1/decisions`, see [Migrating](#migrating-from-v1systemone)).

| The body has | Format |
|---|---|
| `input`, and `questions` is an array | OpenAI |
| `state`, and `questions` is an object | TypeSafe (also Perplexity's) |
| anything else (both, neither, mismatched shape) | `LM-1001`: "send either OpenAI format (`input`, `questions` array) or TypeSafe format (`state`, `questions` object)" |

Either format can be served by any provider kind: LUMEN translates between
them. When the format of the request matches the family of the upstream (a
TypeSafe-format request to Jev or Perplexity), the question bodies and `state`
are sent verbatim, key order included, and the upstream answers come back
verbatim, including unknown fields.

### TypeSafe format

```bash
curl -s http://localhost:8080/v1/decisions \
  -H 'content-type: application/json' \
  -d '{
    "model": "jev",
    "state": "Help! My payouts have been failing for 3 days.",
    "questions": {
      "is_urgent": {
        "type": "noul",
        "instructions": "Does this message convey urgency?"
      },
      "department": {
        "type": "choice",
        "instructions": "Which team should handle this ticket?",
        "criteria": { "technical": "Bugs and outages", "billing": "Payments and invoices" }
      }
    }
  }'
```

`state` is the content to evaluate: a string, an object or an array. Each key
of `questions` is your own question id, and the answer comes back under the
same id.

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "is_urgent": { "type": "noul", "noul": 0.95 },
    "department": {
      "type": "choice",
      "choice": "billing",
      "probabilities": { "technical": 0.12, "billing": 0.88 },
      "confidence": 0.81
    }
  },
  "usage": { "input_tokens": 318, "output_tokens": 34 }
}
```

(The numbers are illustrative.) `model` is the versioned upstream model that
answered (not your configured id). The id that actually served the request,
primary or fallback, is in the `x-lumen-model-used` response header, as for
every capability.

| `type` | Asks | `instructions` | `criteria` |
|---|---|---|---|
| `noul` | A yes/no question; answers the probability of yes. | required, unless `criteria` is given | optional object (what `true` / `false` mean) |
| `choice` | Pick one option; answers the choice, its distribution and a `confidence`. | required | required: an object of 1 to 255 options (option id to description, or `null`) |
| `score` | Rate against ordered levels; answers the level with its legend, distribution and a `confidence`. | required | required: an array of 1 to 10 levels |

### OpenAI format

```bash
curl -s http://localhost:8080/v1/decisions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-6-luna",
    "input": "Help! My payouts have been failing for 3 days.",
    "questions": [
      {
        "type": "predicate",
        "name": "is_urgent",
        "instructions": "Does this message convey urgency?"
      },
      {
        "type": "choice",
        "name": "department",
        "instructions": "Which team should handle this ticket?",
        "choices": [
          { "value": "technical", "description": "Bugs and outages" },
          { "value": "billing", "description": "Payments and invoices" }
        ]
      }
    ]
  }'
```

Answers come back as an array in question order:

```json
{
  "model": "gpt-6-luna",
  "answers": [
    { "type": "predicate", "name": "is_urgent", "probability": 0.95 },
    {
      "type": "choice",
      "name": "department",
      "choice": "billing",
      "probabilities": [
        { "value": "technical", "probability": 0.12 },
        { "value": "billing", "probability": 0.88 }
      ],
      "confidence": 0.88
    }
  ],
  "usage": {
    "input_tokens": 318,
    "input_tokens_details": { "cached_tokens": 0, "cache_write_tokens": 0 },
    "output_tokens": 34,
    "output_tokens_details": { "reasoning_tokens": 0 },
    "total_tokens": 352
  }
}
```

When the upstream is not OpenAI, `usage` is completed to OpenAI's schema so
typed SDKs deserialize it, and `"estimated": true` is added only when LUMEN
estimated the count. The OpenAI SDKs type `confidence` as required on `choice`
and `score` answers: when the upstream gave none, LUMEN uses the highest
probability.

With the OpenAI SDK, point `base_url` at LUMEN (use an SDK release that
includes `client.decisions`):

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="sk-lumen-...")  # any value when auth is off

response = client.decisions.create(
    model="gpt-6-luna",
    input="Help! My payouts have been failing for 3 days.",
    questions=[
        {"type": "predicate", "name": "is_urgent", "instructions": "Does this message convey urgency?"},
    ],
)
print(response.answers[0].probability)
```

| Question `type` | Fields |
|---|---|
| `predicate` | `instructions` (required) |
| `choice` | `instructions`, `choices`: 2 to 255 unique `{value, description?}`; `value` is a string or a boolean |
| `score` | `instructions`, `levels`: at least 1 `{label, description?}` |

`name` is optional but must be unique when present. `safety_identifier`
(at most 128 characters) is forwarded to OpenAI and dropped for other vendors.

## Validation and errors

LUMEN checks the documented request contract itself, **before any upstream
call**, so a malformed question is a precise `400` instead of an opaque
upstream rejection.

- **`LM-2011`** (400): empty `questions` (an empty array or an empty map).
- **`LM-1001`** (400): every other contract violation, with a message naming
  the offending question. A body that is not JSON, or that matches neither
  format; a missing `model`; in the OpenAI format, an unknown top-level
  parameter (as OpenAI itself answers), more than 128 images, a `choice` with
  fewer than 2 or more than 255 options, a duplicate name; in the TypeSafe
  format, a `null` `state`, a missing or unknown question `type`, missing
  `instructions` (unless a `noul` has `criteria`), a malformed `criteria`. A
  request that no target in the chain can take is also `LM-1001`, found before
  any call (see [Cross-vendor fallback](#cross-vendor-fallback)).
- **`LM-2003`** (400): an image sent to a model whose `modalities` lack
  `"image"`. For a virtual model, the message names the leaf model that could
  not take the image, not the virtual id.
- **`LM-2004`** (400): an image that is not a `data:` URL. Remote `http(s)`
  image URLs are refused in both formats.

There is no question-count cap at the gateway edge; per-target caps apply
when a target is chosen (Perplexity 128 questions, Cloudflare 64 questions and
4 images, Ollama `choice` and `score` 2 to 26 options, OpenAI `choice` at
least 2 options). All codes are in [Error codes](../errors.md).

For example, an empty `choice`:

```bash
curl -s http://localhost:8080/v1/decisions \
  -H 'content-type: application/json' \
  -d '{
    "model": "jev",
    "state": "Help! My payouts have been failing for 3 days.",
    "questions": { "department": { "type": "choice", "instructions": "Which team?", "criteria": {} } }
  }'
```

```json
{ "error": { "code": "LM-1001", "message": "question `department`: choice needs 1 to 255 options, got 0", "type": "invalid_request" } }
```

Upstream failures follow the usual mapping (see [Error codes](../errors.md)):

| Upstream status | LUMEN | Retried / falls back |
|---|---|---|
| `401`, `422` | `LM-3003` (502), names the provider; the upstream body is not forwarded | no |
| `429` | `LM-3001` (429), honouring `Retry-After` | yes |
| `529` Overloaded, `5xx` | a retryable 5xx (`LM-3003`) | yes: retries, then the virtual model's next target |

An upstream `401` means the gateway's upstream key is wrong: it surfaces as a
`502` naming the provider, never as a misleading `401` to your client.

## Images

Declare `modalities = ["text", "image"]` on the models that take images (the
default is `["text"]`, so nothing changes silently):

```toml
[[providers.models]]
id = "gpt-6-luna"
capabilities = ["decisions"]
modalities = ["text", "image"]
cost_per_1m_input = 0.10
```

A client sends images in one of two ways at the edge, and LUMEN places them
where each upstream expects them:

| Client format | Where the image goes |
|---|---|
| OpenAI | `input` as user messages with `input_image` parts (`image_url` is a `data:` URL; at most 128 per request) |
| TypeSafe | inside a `state` array, as `{"type": "image_url", "image_url": {"url": "data:..."}}` parts (Perplexity's convention) |

Upstream, the image lands in one of three places, by provider kind: inside
`state` as `image_url` parts (`perplexity`), in a top-level `images` array
(`ollama` with raw base64, `cloudflare`), or as OpenAI's `input_image`
(`openai`). A model without `"image"` in its `modalities` never receives an
image: it is skipped in a fallback chain, or the request fails with `LM-2003`
when nothing is left.

Images make bodies large: one 1024x1024 PNG is about 1 to 3 MiB as base64, and
a request with many images can far exceed the default 10 MiB
`server.body_limit`. Raise it to 32 MiB (Perplexity's own limit) for
deployments that send images:

```toml
[server]
body_limit = 33554432   # 32 MiB
```

The symptom of a too-small limit is a `413` with `LM-1002`.

## Cross-vendor fallback

Any decision model can be a target of the same virtual model. For example,
Perplexity first (cheapest), then Jev, then `gpt-6-luna`:

```toml
[[providers]]
name = "perplexity"
kind = "perplexity"
api_key_env = "PERPLEXITY_API_KEY"
[[providers.models]]
id = "pplx-decider"
upstream_id = "pplx-decider-v1.1-27b"
capabilities = ["decisions"]
modalities = ["text", "image"]
cost_per_1m_input = 0.02

[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"
[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["decisions"]
cost_per_1m_input = 0.042

[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"
[[providers.models]]
id = "gpt-6-luna"
capabilities = ["decisions"]
modalities = ["text", "image"]
cost_per_1m_input = 0.10

[[virtual_models]]
id = "cross"
capability = "decisions"
strategy = "fallback"
targets = [{ model = "pplx-decider" }, { model = "jev" }, { model = "gpt-6-luna" }]
```

Clients send `"model": "cross"` in either format. Before any upstream call,
each target of the chain is checked against the request, and an incompatible
one is **skipped**: it is not an attempt for metrics or `usage_log`, never
counts as a circuit-breaker failure, and is logged at `debug` without content.
If no target is left, the first incompatibility is returned.

| Rule | Who | Error when nothing is left |
|---|---|---|
| An image needs `"image"` in the model's `modalities` | all | `LM-2003` |
| More images than the target allows | Cloudflare (4) | `LM-1001` |
| A `choice` with 1 option | OpenAI, Ollama | `LM-1001` |
| A `choice` or `score` with more than 26 options or levels | Ollama | `LM-1001` |
| More questions than the target allows | Perplexity (128), Cloudflare (64) | `LM-1001` |
| A `noul` without `instructions` | `typesafe` models | `LM-1001` |
| A `choice` with a string and a boolean of the same spelling (`"true"` and `true`) | TypeSafe-family targets | `LM-1001` |

**Refusals.** OpenAI is the only vendor that can answer `{"type": "refusal"}`
for a question. An OpenAI-format client receives it as is. A TypeSafe-format
client receives `{"type": "refusal"}` under that question id, a shape the
TypeSafe and Perplexity SDKs do not know and may fail to deserialize. That only
happens when an operator puts an `openai`-kind target behind a TypeSafe-format
client, as in the `cross` example. Every refusal is counted in
`lumen_decision_refusals_total{model}`. In a rerank remap, a refused
`predicate` or `score` scores 0.0 and a refused listwise `choice` fails the
attempt as `content_filter`, which a virtual model with
`fallback_on = ["content_filter"]` fails over on.

## Token accounting and cost

Every `/v1/decisions` response carries a token count (ADR 003):

- When the upstream reports `usage` with a non-zero `input_tokens`, it is used
  as is, unflagged. A missing or malformed `usage` never fails a billed answer.
- Otherwise the gateway estimates input tokens with its byte heuristic
  (bytes / 4) over all text of the input and every question as sent, plus a
  flat per-image estimate, and flags the response `"estimated": true`. Output
  is estimated as 0 (answers are tiny and not billed); an upstream
  `output_tokens` count is kept when present. Admission reserves the same
  estimate before the upstream call.

Decision models bill **input tokens only**, so price them with
`cost_per_1m_input` and no output price:

| Model | `cost_per_1m_input` |
|---|---|
| Jev (TypeSafe) | 0.042 |
| `pplx-decider-*` (Perplexity) | 0.02 |
| `gpt-6-luna` (OpenAI) | 0.10 |
| Clef (Cloudflare) | 0.24 |
| Ollama (local) | 0 |

OpenAI's `cached_tokens` and `reasoning_tokens` are passed through, not
billed. Prices are the published ones as of 2026-10; set them on your models,
the gateway does not fill them in.

The counts feed `lumen_tokens_total{capability="decisions",direction="input"|"output",...}`
and, when auth is on, `usage_log` rows with `capability = 'decisions'`. Old
rows written under the previous name stay `systemone`:
`GET /admin/usage?capability=` accepts `decisions` or `systemone` and matches
both, and `group_by=capability` shows old rows as `systemone` and new rows as
`decisions`. Virtual keys, RPM/TPM quotas and hard budgets apply exactly as for
the other capabilities. See
[Token accounting & cost](../operations/token-accounting.md) and
[Keys, quotas & budgets](../operations/keys-budgets.md).

**Privacy.** `input`, images, `state`, questions, answers and
`safety_identifier` are request content: they are never logged, never written
to `usage_log`, and never included in errors.

## Local decision models with Ollama

Ollama 0.35 and later serves decision models on `POST /v1/systemone`, with no
auth. Pull a model and add an `ollama` provider:

```bash
ollama pull nimble
```

```toml
[[providers]]
name = "ollama-local"
kind = "ollama"
base_url = "http://localhost:11434"   # the server root, no /v1

[[providers.models]]
id = "nimble"
upstream_id = "nimble"
capabilities = ["decisions"]
```

Limits to know: a `choice` or `score` takes 2 to 26 options or levels; images
are accepted only by `clef` and `clef-flash` (declare their `modalities`
accordingly); unknown request fields are stripped. For a rerank `remap`, an
Ollama target gets about 16k tokens per call and **one call at a time** (a
local GPU), and a listwise `choice` over more than 26 documents is `LM-1001`.
A server that is not running is `LM-3004` and falls back like any unavailable
upstream. A model can take a while to load into VRAM on its first call: relax
`first_token_timeout_ms` and `total_timeout_ms` on the provider block (see
[Providers](../providers.md#ollama-self-hosted)).

## Migrating from /v1/systemone

`POST /v1/systemone`, the TypeSafe-format-only route of 0.6.x, is **removed
in 0.7.0**: it now answers `404 LM-1003` like any unknown route.

1. **Send the same body to `/v1/decisions`.** Nothing else changes in the
   request or the response: `/v1/decisions` detects the TypeSafe format and
   answers in it.
2. **Rewrite your config.** Run `lumen config migrate` (add `--dry-run` to
   preview): it renames the capability `systemone` to `decisions` and the remap
   strategy `noul` to `predicate`. The old spellings still load, with a boot
   warning.
3. **Update dashboards.** The `capability` label of every metric and the
   capability in `GET /v1/models` change from `systemone` to `decisions`. This
   is a breaking change for queries that filter on the old value. `usage_log`
   keeps old rows as they were (see above).

The TypeSafe SDKs call `/v1/systemone`, so they no longer go through LUMEN
from 0.7.0: call `/v1/decisions` with the same body over plain HTTP instead.
Upstream providers are unaffected: the gateway still calls TypeSafe, Ollama
and the other TypeSafe-family vendors on their own `/v1/systemone`.

## Limitations

- **No streaming.** `/v1/decisions` is request/response only.
- **Jev rate limits** (TypeSafe, 2026-09): 64k tokens of context per request
  (32k for `state` plus the longest question), 250k tokens/s and 1200 RPM,
  dynamic. Perplexity: under 262,144 input tokens, 32 MiB body, 10 RPS per
  org. LUMEN does not check context limits itself: a request an upstream
  rejects surfaces as `LM-3003`. To stay under upstream rate limits across
  tenants, set per-key RPM/TPM quotas.
- **Reranking** goes through `/v1/rerank`, not this endpoint: declare a rerank
  virtual model whose target carries a `remap` onto a decision model; see
  [Decision models as rerankers](../reranking/reranking.md#decision-models-as-rerankers-jev-perplexity-openai-ollama-cloudflare).
  Per-tenant relevance rules use a `switch` on the budget group
  ([design notes](../design/systemone-rerank-mapping.md)).

## Providers

Which provider kinds serve `decisions`, with URLs, auth and limits, is in
[Providers](../providers.md#decisions-by-provider-kind).
