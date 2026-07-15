# ADR 005 - Resilience execution model (retries, fallback, circuit breaker, timeouts)

- Status: accepted (amended 2026-07-15: per-provider connect timeout and first-frame-peek streaming retry, see below)
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

- **connect** (default 5 s) - a `reqwest::Client` setting. Originally **global
  only** (per-provider connect would require one client per provider and lose
  connection pooling; deferred). **Superseded by the 2026-07-15 amendment
  below**: a provider may now override it with `connect_timeout_ms`, at the cost
  of its own (unpooled) client. A connect timeout is distinguished from a read
  timeout: `ProviderError::ConnectTimeout` → `LM-3012` (504).
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
  explicitly out of scope here and recorded in `docs/backlog.md`. (Per-provider
  connect timeouts were subsequently implemented; see the amendment below.)

## Amendment (2026-07-15): per-provider connect timeout

The original decision left `connect` global-only because a per-provider connect
timeout is a `reqwest::Client` setting and one pooled client is shared across
all providers, so overriding it per provider would mean one client per provider
and the loss of cross-provider connection pooling. Issue #24 asked for it anyway
(an upstream that is reliably reachable can afford a much tighter connect
deadline than a flaky one; a distant self-hosted box may need a looser one), so
the deferral is **superseded**.

**Chosen design.** The shared, pooled client remains the default and carries the
global `resilience.connect_timeout_ms`. A provider that sets the new optional
`connect_timeout_ms` (alongside the existing per-provider `first_token_timeout_ms`
and `total_timeout_ms`) is given its **own** `reqwest::Client`, built once at
registry construction, with that connect timeout and the *same* overall backstop
as the shared client. Every provider that does not override keeps sharing the
one pooled client, so pooling is preserved for the common case and only an
explicitly-overriding provider pays for it.

**Trade-off (documented, accepted).** An overriding provider no longer shares
the process-wide connection pool: its connections are pooled only within its own
dedicated client. This is a per-provider, opt-in cost. Nothing else about the
provider changes (same overall cap, same executor timeouts, same error codes).

**Where it lives.** `ProviderSpec` gains `connect_timeout_ms: Option<u64>`;
`Registry::build`/`reload` take the overall backstop and build the dedicated
clients in `build_inner`. Because the registry rebuilds all clients from the new
specs on every hot reload, changing (adding, editing or removing) a provider's
`connect_timeout_ms` takes effect on `SIGHUP`/file-change reload with no restart,
exactly like the other two per-provider timeout overrides. Config validation
rejects a `connect_timeout_ms` of `0`, matching the other overrides.

---

## Amendment - 2026-07-15: first-frame-peek streaming retry (issue #7)

Supersedes the "2xx-then-immediate-error is deliberately treated as committed"
carve-out above (the parenthetical in *Streaming: retry/fallback only at the
open phase*) and the matching `docs/backlog.md` deferral. The rest of the
original decision stands unchanged.

### Decision

The commitment point for a streaming response moves from the **open** (2xx +
headers) to the **first content frame**. After the open succeeds, the streaming
closure PEEKS the first upstream frame before committing:

- first item is a content frame (`Ok`) - **commit**: the peeked frame is
  re-attached ahead of the untouched remainder (a single `Bytes`, moved not
  copied) and the reconstructed stream is handed to the existing
  `to_event_stream` guards. From here nothing retries (mid-stream errors still
  become a terminal SSE error frame - LM-3010/LM-3003 - exactly as before);
- first item is an error (`Err`), or the stream ends before any frame (`None`) -
  a **pre-commit failure**: the closure returns `Err`, so the executor retries,
  falls over to the next link, and charges the circuit breaker *identically to
  an open failure*. A `None` is surfaced as the new
  `ProviderError::EmptyStream` (retryable, provider-fault, mapped to LM-3010).

### Boundaries and invariants preserved

- **Zero-copy / bounded buffering (pillar 1, ADR 004).** The peek buffers **at
  most one frame**, never the stream. The committed body is byte-identical to
  the upstream.
- **Cancellation.** The peek races the request `CancellationToken`; a client
  disconnect during the peek window returns `ProviderError::Cancelled` and drops
  the stream, aborting the upstream. `Cancelled` is neither retryable nor a
  provider fault, so no fallback is attempted and the breaker is untouched.
- **Time bound.** The peek runs inside the closure the executor already wraps in
  the per-attempt `first_token` timeout, so a silent upstream (headers, then no
  bytes) trips `FirstTokenTimeout` and fails over rather than hanging.
- **Still non-retryable (unchanged): everything post-commit.** Once a content
  frame is forwarded, a later mid-stream error, a missing `[DONE]` (LM-3010) or a
  first-token gap on a subsequent frame is a terminal SSE error, never a retry.

### Consequence for the breaker note above

The recorded trade-off ("the breaker only ever sees the open phase") is now
narrower: a provider that opens 200 then fails on its **first** frame *does* now
count against its breaker. Only failures *after* the first committed content
frame remain invisible to the breaker (the frame guards still give the client a
clean terminal error each time).

The peek lives in `crates/router/src/peek.rs` (generic over the frame type,
unit-tested for commit / error / empty / cancel / timeout); wiremock acceptance
tests are in `crates/server/tests/resilience.rs`.
