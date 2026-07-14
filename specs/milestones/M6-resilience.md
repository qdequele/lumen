# M6 — Resilience

## Objective
Retries, fallbacks, circuit breaker, timeouts — without ever compromising the stability of the gateway itself under load (LiteLLM lesson #15526: cascade of k8s restarts under upstream 429s).

## Tasks

### 6.1 Retries
- [x] Retry only on retryable `ProviderError` (5xx, connect timeout, 429) — never on client 4xx
- [x] Exponential backoff + jitter (default: base 200 ms, max 5 s, 3 attempts), honors `Retry-After` if it is longer
- [x] Streaming: retry ONLY if no chunk has been emitted to the client yet
- [x] Global retry budget per request (the total time stays bounded by the total timeout)

### 6.2 Fallback
- [x] Config: `fallbacks = ["model-a", "model-b"]` per model — same capability required, validated at boot
- [x] Fallback triggered after the current provider's retries are exhausted
- [x] `x-lumen-model-used` response header + field in usage_log
- [x] Same streaming rule: no fallback after the first emitted chunk

### 6.3 Circuit breaker
- [x] Per (provider, model): Closed → Open after N consecutive failures (default 5) → Half-Open after cooldown (default 30 s) → 1 probe request
- [x] Open circuit → immediate skip to the fallback; if no fallback: 503 LM-3020 with Retry-After
- [x] State exposed in /metrics (`circuit_state{provider,model}`)

### 6.4 Timeouts
- [x] Three configurable timeouts per provider with global defaults: `connect` (5 s), `first_token` (30 s), `total` (600 s)
- [x] Each timeout → a distinct error (LM-3011/3012/3013) for debugging

Note: `connect` is a global client setting (a single shared HTTP client) —
no per-provider override; `first_token` and `total` are overridable per
provider. LM-3011 = first-token, LM-3012 = connect, LM-3013 = total. See
`docs/adr/005-resilience-execution.md`.

### 6.5 Background health checks
- [x] Optional periodic task (default off) that probes the providers — results in memory + metric, NEVER consulted in the request path in a blocking way
- [x] The gateway's /health stays independent of provider health; add a separate `/health/providers` for observability

## Acceptance criteria
1. Test: upstream 500 then 500 then 200 → success, 3 wiremock calls, backoff delays respected (simulated time start_paused).
2. Test: provider A exhausts its retries → switches to B, response OK, header x-lumen-model-used = B.
3. Test: 5 failures → circuit Open → the 6th request does NOT touch the upstream (wiremock counter) and falls back immediately; after cooldown, 1 probe passes.
4. Streaming test: failure after 2 emitted chunks → NO retry or fallback, clean SSE error.
5. Load test: 500 concurrent requests to an upstream that responds 429 → /health responds < 10 ms throughout, stable RAM (no unbounded queue).
6. Test: Retry-After: 3 → wait of at least 3 s (simulated time).
