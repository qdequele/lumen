# ADR 021 - Upstream cold-start retry (503 wait hints, per-provider retry policy)

- Status: accepted
- Date: 2026-11-08
- Amends: [ADR 005](005-resilience-execution.md) (retry loop)

## Context

The Lab production audit of 2026-11-08 (item 16) found that the first
`/v1/embeddings` call to `hf-bge-m3/bge-m3` after idle failed with 502
`LM-3003` ("upstream provider 'hf-bge-m3' returned an error (HTTP 503)"), and an
immediate manual retry succeeded in 300 ms. The model sits on a scale-to-zero
Hugging Face endpoint: the first request wakes it and is answered 503 until a
replica is up. A Meilisearch embedder on such a model fails its first indexing
batch after every idle period.

The gateway already retried the 503 (ADR 005: every 5xx is retryable), but with
the global policy (3 attempts, 200 ms base, 5 s cap) the three attempts all land
within about 600 ms, long before the endpoint has woken up. Two pieces of
information that would have helped were thrown away:

1. `Retry-After` on a 503. `classify_status` kept it only for 429.
2. The body of a 503. 5xx bodies were never read, so Hugging Face's
   loading response (`{"error": "... is currently loading", "estimated_time":
   20.0}`) was invisible.

And there was no way to give one slow-to-wake provider a longer retry policy
without lengthening it for every provider.

## Decision

### The 503 wait hint

`ProviderError::Upstream` gains `retry_after: Option<Duration>`, set only for a
**503** (RFC 9110 defines `Retry-After` on 503 and 429; on any other 5xx it is
ignored). Its value is the `Retry-After` header (delta-seconds) when present,
otherwise the body's top-level `estimated_time` (Hugging Face's loading
response, seconds as a float, rounded up to whole milliseconds). A non-zero
header wins over the body; a `Retry-After: 0` says nothing, so a body estimate
replaces it. `ProviderError::retry_after()` now returns it for `Upstream` as
well as `RateLimited`, so the existing retry loop floors the backoff at it
unchanged.

To see `estimated_time`, a 503 body is now read, through the same bounded path
as 4xx bodies (16 KiB, 500 ms deadline, never returned to the client, only its
redacted message logged). Other 5xx bodies are still skipped, so a stalled body
never delays their retry.

### A cap on any upstream wait hint

A hint is untrusted input. A new knob, `retry_after_max_ms` (default 60 000),
bounds it: when an upstream asks for a longer wait than the cap, the gateway
does **not** retry that link. It returns the error at once so a fallback (if
the virtual model has one) or the client can act, instead of holding the
connection open. This applies to 429 `Retry-After` too; before this change a
429 `Retry-After: 300` held the request for five minutes (bounded only by the
total timeout).

### Never sleep past the total deadline

The retry loop now receives the request's total deadline. When the next backoff
would end after it, the loop returns the upstream error instead of sleeping
into a certain `LM-3013`. The client sees the real cause (`LM-3003` naming the
provider and the 503), and a fallback link still gets the remaining time it can
use. A cheap `Instant::now()` on the failure path only; the success path is
unchanged.

### Per-provider retry policy

A provider may override `retry_max_attempts`, `retry_base_ms`, `retry_max_ms`
and `retry_after_max_ms`; unset fields inherit `[resilience]`. The override is
carried on each executor `Link` (`Link::retry`), filled by the server from the
link's provider after the chain is resolved, so in a virtual model each link is
retried by its own provider's policy. A scale-to-zero endpoint without wait
hints can be given, for example, `retry_max_attempts = 6`, `retry_base_ms =
1000`, `retry_max_ms = 15000`: up to about 30 s of backoff, on that provider
only. Overrides follow hot reload like the timeout overrides.

### What is retried, and billing

Unchanged from ADR 005: only retryable failures are retried (5xx, timeouts,
unreachable, 429), never a client 4xx, and a stream only before its first
content frame reaches the client. Embeddings and rerank are idempotent; a chat
stream is retried only pre-commit; a non-streaming chat retry follows a failure
response, so nothing was returned to the client.

A logical request is admitted (budget reservation) once before the executor and
settled once after it, with the usage of the attempt that succeeded. A failed
attempt records no usage, so a retry never bills twice, and the circuit breaker
still records one outcome per link.

## Consequences

- A cold-start 503 that carries `Retry-After` or `estimated_time` is waited out
  automatically (up to the cap), with no configuration.
- A 503 body is now read (bounded) and its message logged at `warn`, like a 4xx.
- A 429 or 503 asking for more than `retry_after_max_ms` is no longer waited
  for; it fails over or is returned at once. Operators who relied on long 429
  waits raise the cap.
- `docs/openapi.yaml` gains the four optional, nullable provider fields
  (additive contract change).
- No new error code, no DB access, no new dependency.
