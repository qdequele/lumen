# SystemOne (typed decisions)

`POST /v1/systemone` answers typed questions about a `state` with calibrated
probabilities, instead of generating text. It speaks TypeSafe's request and
response format unchanged ([TypeSafe API](https://docs.typesafe.ai/api)), so
it is served by the `typesafe` provider kind (Jev models) and existing
TypeSafe SDK clients work against LUMEN with no code change (see
[TypeSafe SDK drop-in](#typesafe-sdk-drop-in)). The `model` field is one of
*your* configured model ids (the `id` in a `[[providers.models]]` block
declaring `capabilities = ["systemone"]` - see [Providers](../providers.md#typesafe)).

The design and its trade-offs are in
[ADR 013](../adr/013-systemone-capability.md).

## Configuration

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
```

## Request

```bash
curl -s http://localhost:8080/v1/systemone \
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
of `questions` is your own question id; the answer comes back under the same
id. `state`, every question body and any unknown top-level field are
forwarded to the upstream byte-for-byte, so JSON key order (visible to the
model, e.g. the order of `choice` options) is preserved.

## Response

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

`model` is the versioned upstream model that answered, as reported by
TypeSafe (not your configured id). The `answers` map is passed through
verbatim: LUMEN never interprets or reshapes an answer. The model id that
actually served the request (primary or a fallback) is in the
`x-lumen-model-used` response header, as for every capability.

## Question types

| `type`   | Asks                                  | `instructions` | `criteria`                                                        |
|----------|---------------------------------------|----------------|-------------------------------------------------------------------|
| `noul`   | A yes/no question; answers the probability of yes. | required (non-null) | optional; if present, an object (e.g. what `true` / `false` mean) |
| `choice` | Pick one option from a defined set; answers the choice, its distribution and a `confidence`. | required (non-null) | required; an object of 1 to 255 options (option id to description, or `null`) |
| `score`  | Rate the state against ordered levels; answers the level with its legend, distribution and a `confidence`. | required (non-null) | required; an array of 1 to 10 levels, e.g. `["Calm", "Frustrated", "Very angry"]` |

`instructions`, the entries of `criteria` and `state` stay opaque JSON:
strings, objects and arrays are all accepted as TypeSafe accepts them.

## Validation and errors

LUMEN checks the documented request contract itself, **before any upstream
call**, so a malformed question is a precise `400` instead of an opaque
upstream rejection:

- An empty `questions` map is rejected with `LM-2011` (400), the SystemOne
  twin of rerank's `LM-2010`.
- Every other contract violation is `LM-1001` (400), with a message naming the
  offending question: a `null` `state`; a question that is not an object; a
  missing `type`; an unknown `type` (only `noul`, `choice` and `score` are
  accepted, a new TypeSafe question type needs a LUMEN upgrade); missing or
  `null` `instructions`; `noul` `criteria` that is not an object; `choice`
  without `criteria`, with non-object `criteria`, or with 0 or more than 255
  options; `score` without `criteria`, with non-array `criteria`, or with 0 or
  more than 10 levels. A body missing `model`, `state` or `questions`, or whose
  `questions` is not an object, is also `LM-1001`.

For example, a `choice` question with no options:

```bash
curl -s http://localhost:8080/v1/systemone \
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

| TypeSafe status       | LUMEN                                        | Retried / falls back |
|-----------------------|----------------------------------------------|----------------------|
| `401`, `422`          | `LM-3003` (502), names the provider; the upstream body is not forwarded | no |
| `429`                 | `LM-3001` (429), honouring `Retry-After`     | yes                  |
| `529` Overloaded      | a retryable 5xx                              | yes: retries, then `fallbacks` |

A TypeSafe `401` means the gateway's upstream key is wrong: it surfaces as a
`502` naming the provider, never as a misleading `401` to your client.

## Billing and token accounting

Jev bills **input tokens only**: $0.042 per 1M input tokens, output free
(TypeSafe pricing, 2026-09). Price it with the existing per-token fields; no
output price is needed:

```toml
[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone"]
cost_per_1m_input = 0.042
```

Every `/v1/systemone` response carries a token count (ADR 003):

- When TypeSafe reports `usage` with a non-zero `input_tokens`, it is used as
  is, unflagged.
- Otherwise the gateway estimates input tokens with its byte heuristic
  (bytes / 4) over the JSON of `state` plus every question body, and flags the
  response `"usage": { "input_tokens": ..., "output_tokens": 0, "estimated": true }`.
  Output is estimated as 0 (answers are tiny and not billed); an upstream
  `output_tokens` count is kept when present.

The counts feed `lumen_tokens_total{capability="systemone",direction="input"|"output",...}`
and, when auth is on, `usage_log` rows with `capability = 'systemone'`
(`GET /admin/usage?capability=systemone` filters them). Virtual keys, RPM/TPM
quotas and hard budgets apply exactly as for the other capabilities:
admission reserves the input estimate before the upstream call, so a request
over budget is rejected without spending. See
[Token accounting & cost](../operations/token-accounting.md) and
[Keys, quotas & budgets](../operations/keys-budgets.md).

**Privacy.** `state`, questions and answers are request content: they are
never logged, never written to `usage_log`, and never included in errors.

## Models and upstream limits

TypeSafe exposes `jev-latest` (stable alias), `jev-preview` (preview alias)
and versioned ids such as `jev-1.13.0`. Map them with `upstream_id`; several
of your ids may point at the same upstream id.

TypeSafe documents (2026-09) a context of 64k tokens per request, of which at
most 32k for `state` plus the longest question, and upstream rate limits of
250k tokens/s and 1200 RPM (dynamic). LUMEN does not check the context limit
itself: a request TypeSafe rejects surfaces as `LM-3003`. To stay under the
upstream rate limits across tenants, set per-key RPM/TPM quotas.

## Fallbacks

Like any capability, a SystemOne model can list `fallbacks`. A common setup
pins a version for reproducible answers and falls back to the stable alias
when that version is overloaded (`529`) or its circuit is open:

```toml
[[providers.models]]
id = "jev-1.13.0"
upstream_id = "jev-1.13.0"
capabilities = ["systemone"]
cost_per_1m_input = 0.042
fallbacks = ["jev"]
```

Each fallback must exist and serve `systemone` (validated at boot). See
[Resilience](../operations/resilience.md).

## TypeSafe SDK drop-in

The TypeSafe SDKs read their base URL from `TYPESAFE_BASE_URL` and their key
from `TYPESAFE_API_KEY`. Point them at LUMEN and existing code goes through
the gateway unchanged:

```bash
export TYPESAFE_BASE_URL=http://localhost:8080   # the LUMEN base URL, no /v1
export TYPESAFE_API_KEY=sk-lumen-...             # a LUMEN virtual key (any value when auth is off)
```

The `model` your code sends must be one of your configured ids. To keep code
that sends TypeSafe's own ids untouched, expose ids that match them:

```toml
[[providers.models]]
id = "jev-latest"
capabilities = ["systemone"]
cost_per_1m_input = 0.042
```

With those variables set, the TypeSafe Python SDK (`pip install typesafe-sdk`,
default model `jev-latest`) goes through LUMEN unchanged:

```python
from typesafe_sdk import Choice, Noul, TypeSafeClient

with TypeSafeClient() as client:
    response = client.system_one(
        state={"document": "I was charged twice. Please fix this ASAP."},
        questions={
            "billing": Noul(instructions="Is this ticket about billing?"),
            "tone": Choice(
                instructions="What is the customer's tone?",
                criteria={"calm": None, "frustrated": None, "angry": None},
            ),
        },
    )
print(response.nouls["billing"].noul)
print(response.choices["tone"].choice)
```

Code without the SDK uses the same URL and bearer key: `POST
$TYPESAFE_BASE_URL/v1/systemone`.

## Limitations

- **`models.list()` does not work through LUMEN.** `GET /v1/models` keeps
  LUMEN's OpenAI list shape (each SystemOne model is listed with
  `"capabilities": ["systemone"]`), which the TypeSafe SDKs do not parse. Every
  other SDK call goes to `/v1/systemone` and works.
- **Jev as a reranker** goes through `/v1/rerank`, not this endpoint: declare
  a `typesafe` model with `capabilities = ["rerank"]`; see
  [Jev as a reranker](../reranking/reranking.md#jev-as-a-reranker-typesafe).
  Per-key or per-tenant converters are not supported yet
  ([design notes](../design/systemone-rerank-mapping.md)).
- **No streaming.** `/v1/systemone` is request/response only, like TypeSafe's
  endpoint.

## Providers

Which provider kinds serve `systemone` and their setup is in
[Providers](../providers.md#typesafe).
