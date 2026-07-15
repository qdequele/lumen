# Resilience tuning

LUMEN survives flaky upstreams without becoming flaky itself. A request goes
through, in order: **retries**, then **fallback** to the next model in the
chain, then the **circuit breaker** deciding whether to even try a given
provider. None of this touches the database on the request path. Retries and
the circuit breaker are always active with sane defaults; only per-model
`fallbacks` and background health checks are opt-in. All of it lives under
`[resilience]` - see [ADR 005](../adr/005-resilience-execution.md) for the
design.

## Retries

Applied **only** to retryable upstream failures: 5xx, connect/read timeouts,
and 429. A client 4xx is **never** retried - a fallback provider would reject
it too. Backoff is exponential with **equal jitter**, and honors an upstream
`Retry-After` header as a floor (a `Retry-After: 3` guarantees at least a
3-second wait). While streaming, a retry only happens if the upstream byte
stream hasn't opened yet - once a chunk has reached the client, the request
is committed and errors surface as a clean SSE error frame instead.

```toml
[resilience]
retry_max_attempts = 3   # total attempts per provider incl. the first (>= 1)
retry_base_ms = 200      # base backoff wait after the first failure
retry_max_ms = 5000      # ceiling on the exponential backoff term
```

Set `retry_max_attempts = 1` to disable retries entirely.

## Fallback chains

Each model can declare `fallbacks`, a list of other model ids to try in
order if the primary fails. Fallback chains are validated **at boot**: every
fallback id must exist and serve the same capability as the model it backs,
so a runtime resolution miss never happens in practice. Whichever model
actually served a request is reported in the `x-lumen-model-used` response
header, so a caller (and the usage log) can see when a fallback fired.

## Circuit breaker

Tracked per `(provider, model)` pair:

```toml
circuit_failure_threshold = 5     # consecutive failures that trip it open
circuit_cooldown_ms = 30000       # time spent open before a half-open probe
```

After `circuit_failure_threshold` consecutive provider-fault failures, the
circuit opens. While open, that link is skipped instantly - no upstream
call - straight to the next fallback, or answered with `503 LM-3020` (plus
`Retry-After` set to the cooldown remainder) if none remains. After
`circuit_cooldown_ms`, a single half-open probe decides whether to close
again. State is exported as `lumen_circuit_state{provider,model}` (`0`
closed, `1` open, `2` half-open).

## The three timeouts

| Timeout | Code | Where configured |
|---|---|---|
| Connect | `LM-3012` (504) | Client-wide - one pooled `reqwest::Client`, so there is no per-provider connect override. |
| First-token | `LM-3011` (504) | `[server].first_token_timeout_ms`; per-provider overridable. Streaming: time to the first SSE frame. Non-streaming: the whole call. |
| Total | `LM-3013` (504) | `[resilience].total_timeout_ms`; per-provider overridable. Bounds the entire request - all retries and fallbacks together. |

```toml
[resilience]
connect_timeout_ms = 5000
total_timeout_ms = 600000   # 10 minutes
```

## Health checks

Off by default. When `health_check_enabled = true`, a background task
probes, on `health_check_interval_ms`, every provider that has an explicit
`base_url` (self-hosted TEI/Ollama, or any explicit override) - providers on
a built-in vendor URL are never probed and report `unknown`, since the
gateway hardcodes no vendor endpoints. Results are published at `GET
/health/providers` and the `lumen_provider_up{provider}` gauge. This is
independent of the gateway's own liveness: `GET /health` never depends on
provider state and does no I/O.

```toml
health_check_enabled = false
health_check_interval_ms = 30000
```

## How this shapes the error codes you see

Retries, fallback and the circuit breaker all happen before a `3xxx` error
ever reaches a client - by the time one surfaces, the resilience machinery
has already given up. See [Error codes: "How resilience shapes these
codes"](../errors.md#how-resilience-shapes-these-codes) for the full mapping
from a given upstream failure to the code you'll see.
