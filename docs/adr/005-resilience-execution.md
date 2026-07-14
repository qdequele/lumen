# ADR 005 - Resilience execution model (retries, fallback, circuit breaker, timeouts)

- Status: accepted
- Date: 2026-07-13

## Context

The resilience layer adds retries, multi-provider fallback chains, a
per-provider circuit breaker and per-phase timeouts. The overriding constraint
is pillar 1 (< 1 ms added p99) and pillar 3 (robustness): none of this may add a
database or lock to the request path, and - the explicit lesson (LiteLLM
#15526) - a storm of 429s from an upstream must never destabilise the gateway
itself. The router crate was previously a pure resolution helper; this change
turns it into an **execution** layer that wraps a provider call with the
resilience machinery.

Three design tensions had to be resolved:

1. **Where the machinery lives, given three capability traits.** `ChatProvider`,
   `EmbeddingProvider` and `RerankProvider` have different signatures, so a
   single concrete executor cannot call all three. Duplicating retry/breaker
   logic per capability is unacceptable.
2. **Streaming.** Retry and fallback are only safe *before the first byte
   reaches the client* (spec 6.1/6.2/6.4). Once a frame is forwarded the request
   is committed.
3. **Jitter vs. deterministic tests.** Backoff needs randomness, but the
   acceptance criteria assert on elapsed (simulated) time.

## Decision

### One generic executor, capability-specific chain resolution

The executor is **generic over a closure** `FnMut(link_index) -> Future<Result<T,
ProviderError>>`. It owns the resilience control flow - breaker gate, retry
loop, fallback across links, total-timeout deadline - and knows nothing about
chat/embed/rerank. Each handler resolves a **chain** (`resolve_*_chain`: the
requested model followed by its configured fallbacks, each re-resolved for the
*same* capability) and supplies a closure that performs the actual typed call
for a given link. The chain the executor sees is metadata only
(`provider_name`, `model_id`), which it uses to key the circuit breaker and to
report the model that actually served (`x-lumen-model-used`).

Fallback chains are validated **at boot**: every fallback id must exist and
serve the same capability as the model it backs (spec 6.2). A runtime
resolution miss is therefore not expected, but is treated as a skipped link
rather than a panic.

### Retry classification lives on `ProviderError`

`ProviderError::is_retryable()` (5xx, connect/read timeout, 429, unreachable -
never a 4xx client fault) and `is_provider_fault()` (does this failure indicate
the *provider* is unhealthy, i.e. should it count against the breaker) are the
single source of truth, shared by the retry loop and the breaker. A hard
upstream 4xx (bad request) is neither retried nor failed over - a fallback
provider would reject it too - and is returned immediately.

### Backoff: pure function + injected randomness

`backoff_delay(attempt, policy, retry_after, rand01)` is a **pure function**:
exponential base·2ⁿ capped at `max`, **equal jitter** (`d = e/2 + e/2·rand01`, so
`d ∈ [e/2, e]`), then floored at `Retry-After` when the upstream sent a longer
one. Production passes `rand01` from a cheap lock-free splitmix64 (no dependency,
no blocking, no `Instant::now` on the hot path); tests call the pure function
with fixed fractions and assert exact bounds. The equal-jitter floor (`e/2`)
makes "backoff delays respected" assertions robust regardless of the random
draw, and the `Retry-After` floor makes criterion 6 (`Retry-After: 3` ⇒ ≥ 3 s)
hold unconditionally.

### Circuit breaker: in-memory, per (provider, model)

A lock-free-ish `CircuitBreaker` (a `Mutex<small struct>` per key, never held
across an `.await`) transitions Closed → Open (after N consecutive
provider-fault failures) → Half-Open (after the cooldown) → one probe →
Closed/Open. Concurrent requests that find the breaker Half-Open are refused the
probe (treated as Open) so exactly one request probes. State is pushed to a
Prometheus gauge `lumen_circuit_state{provider,model}` (0 closed / 1 open /
2 half-open) on every transition - the telemetry crate exposes a numeric setter
so `router` depends on `telemetry` with no cycle. The breaker map is a
`DashMap`, entries created on first use; bounded by the (provider × model) count,
which is operator-configured and finite. **Never touched off the request path by
a blocking call** - health checks read a snapshot.

A logical request records **one** breaker outcome per link (success, or one
failure after that link's retries are exhausted), so "5 consecutive failures"
means five requests, not five retries within one.

### Streaming: retry/fallback only at the open phase

`execute_stream` retries and falls back around **opening** the upstream byte
stream (send + status check, before any body). Once the stream opens (upstream
returned 2xx and we commit to forwarding), the existing `to_event_stream` guards
(ADR 004: LM-3010 missing terminator, LM-3011 first-token, heartbeat) own the
rest and never retry. This satisfies "retry only if no chunk emitted": an open
failure means nothing was forwarded, a post-open failure becomes a clean SSE
error frame. (A 2xx-then-immediate-error is deliberately treated as committed -
not retried - since the upstream accepted the request.)

One consequence, recorded explicitly: the circuit breaker for a streaming call
only ever sees the **open** phase. `on_success` fires as soon as the byte stream
opens, so a provider that opens cleanly but then dies mid-stream every time
(LM-3010, handled by the frame guards and never surfaced back to the breaker)
will *not* trip its circuit. This is the accepted trade-off of the open-phase
boundary; the frame guards still give the client a clean terminal error each
time.

**Half-open cannot wedge.** A half-open probe is normally resolved by
`on_success`/`on_failure`, but a probe whose result never returns (client
disconnect, the total-timeout firing mid-probe, or a non-provider-fault error
that does neither) must not pin the breaker shut. The breaker records when a
probe was admitted and **auto-rearms**: a probe still outstanding after a full
cooldown is presumed lost and a fresh one is admitted. The single-probe
guarantee therefore holds *within* a cooldown window, and recovery is always
self-healing.

### Timeouts and new error codes

Three timeouts, global defaults with per-model overrides:

- **connect** (default 5 s) - a `reqwest::Client` setting, so it is **global
  only** (per-provider connect would require one client per provider and lose
  connection pooling; deferred, noted in docs). A connect timeout is now
  distinguished from a read timeout: `ProviderError::ConnectTimeout` →
  `LM-3012` (504).
- **first_token** (default 30 s, per-model override) - reuses the streaming path;
  `LM-3011` (504). For non-streaming it bounds the whole call; for streaming,
  the time to the first frame.
- **total** (default 600 s, per-model override) - an absolute deadline threaded
  through the executor bounding *all* retries and fallbacks together; exceeding
  it yields `LM-3013` (504).

Circuit open with no fallback left is `LM-3020` (503) carrying `Retry-After`
(the cooldown remainder). `LM-3004` (no healthy upstream) already existed and is
kept for "all fallbacks exhausted".

### Health checks: optional, off the request path

A background task (default **off**) probes each provider that has a configured
`base_url` (self-hosted TEI/Ollama, or any explicit override) with a short GET,
storing Up/Down + latency in memory and a `lumen_provider_up{provider}`
gauge. Providers relying on a built-in vendor URL report `unknown` - the gateway
never hardcodes vendor endpoints. Results are exposed at **`/health/providers`**
for observability; the gateway's own **`/health`** stays completely independent
of provider health (criterion 5) and does no I/O.

## Consequences

- Handlers change from `route.provider.chat(...)` to
  `execute_unary(chain, ..., |i| chain[i].provider.chat(...))`; the resilience
  policy is uniform across all three capabilities and both streaming modes.
- `router` gains a dependency on `telemetry` (gauge) and on `tokio`/`futures`
  (it is now async). No dependency cycle.
- The circuit-breaker map and health results are process-wide in-memory state in
  `AppState`; nothing resilience-related touches SQLite on the request path.
- `usage_log` gains a `model_used` column (migration 0002) so a fallback is
  observable after the fact, mirroring the `x-lumen-model-used` header.
- Per-provider connect timeouts and true first-frame-peek streaming retries are
  explicitly out of scope here and recorded in `docs/backlog.md`.
