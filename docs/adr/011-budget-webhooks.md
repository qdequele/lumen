# ADR 011 - Outbound webhooks for budget events

- Status: accepted
- Date: 2026-08-25
- Tracking issue: #146

## Context

A billing control plane integrating with LUMEN already has two of the three
legs a budget loop needs:

- **Control (push, backend to gateway)**: `POST /admin/keys/{id}/grant` and
  `POST /admin/groups/{id}/grant` apply atomic budget top-ups (ADR 009);
  `PATCH` adjusts limits and enable/disable.
- **Reconciliation (pull, backend from gateway)**: `GET /admin/usage/export`
  streams invoice-grade raw usage rows (ADR 010).

The third leg, signals from the gateway to the backend, does not exist. A
backend that sells prepaid credit (e.g. metering through Stripe) can only
poll to learn that a customer's budget is nearly consumed. Hard budgets make
that a poor fit: the moment the pool reaches zero, the customer's requests
are refused with 402 `LM-4001`. An auto-recharge must fire before that
moment, and polling trades latency against load in a way webhooks do not.

Constraints inherited from the pillars and prior ADRs:

- Nothing new on the request path; the database is never consulted per
  request and neither is any network endpoint that is not the routed
  provider (pillar 1, rule 4).
- Zero telemetry by default: today the gateway makes outbound calls only to
  configured providers. Any new outbound traffic must be strictly opt-in and
  carry no prompt or response content (pillar 2, `usage_log`'s no-content
  construction).
- Budget enforcement and settlement are in-memory atomics, flushed to
  SQLite on an interval; a crash loses at most `flush_interval_ms` of
  accounting (M5, ADR 007).
- The admin API remains the one control surface; ADR 010's console pulls
  and pushes through it and reports drift rather than healing it.

## Decision

An opt-in **webhook sender** for budget lifecycle events, delivered from the
accounting layer, never from the request path.

### 1. Events

Three event families, all derived from state the gateway already tracks:

- `budget.threshold`: a key's or group's `budget_spent / budget_max` crossed
  a configured percentage. Thresholds are configurable
  (`thresholds = [50, 80, 95]`); each crossing is **edge-triggered**: one
  event per threshold per budget epoch, not one per request beyond it. A
  `grant` that drops consumption back under a threshold re-arms it (a new
  epoch begins when `budget_max` changes).
- `budget.exhausted`: the first admission refused with `LM-4001` for a key
  or group since it last had budget. Also edge-triggered.
- `key.disabled`, `key.rotated`, `key.deleted`: administrative lifecycle
  changes, so a backend registry can stay in sync instead of discovering
  drift on the next reconciliation pull (ADR 010 surfaces drift; these
  events shrink the window in which it exists).

The payload carries accounting facts only: event type, an event id, the key
or group id and name, `budget_max`, `budget_spent`, the crossed threshold,
and a timestamp. Never a plaintext key, never metadata, never content.

### 2. Detection is free, delivery is decoupled

Budget settlement already performs an atomic `fetch_add` per request.
Threshold detection adds a compare of the before/after values against the
armed thresholds on that same settle, in memory. When a crossing is
detected, the event is pushed into a **bounded mpsc channel** consumed by an
async sender task, exactly the usage-log writer pattern (rule 4):

- A full channel **drops the event** and increments
  `lumen_webhook_dropped_total`. Webhooks are a convenience signal, not an
  accounting system; the export route remains the source of truth and the
  backend must reconcile against it.
- The sender delivers with **at-least-once** semantics: bounded exponential
  backoff (with jitter, capped attempts), then the event is dropped and
  `lumen_webhook_dead_total` increments. Delivery state is memory-only; a
  restart forgets undelivered events. This is deliberate: persisting a
  delivery queue would put a new writer on the DB and buy little, since the
  reconciliation pull already covers gaps.
- Shutdown cancels the sender without blocking: in-flight attempts are
  aborted through the same CancellationToken discipline as provider calls.

### 3. Authenticity and idempotency

Each delivery is a `POST` with:

- `x-lumen-signature`: HMAC-SHA256 over the raw body, keyed by a secret read
  from an env var named in config (`signing_key_env`, the provider-key
  pattern; the secret is never logged, never in errors, never in Debug).
- `x-lumen-event-id`: a unique id per event (not per attempt), so retries
  and post-crash re-fires are deduplicable by the receiver.
- `x-lumen-timestamp`: to let receivers reject stale replays.

Because a crash can lose up to `flush_interval_ms` of settled accounting,
the same threshold can legitimately re-fire after a restart. Receivers must
treat events as idempotent signals (the Stripe webhook contract), and grant
routes are already atomic increments, so a duplicated auto-recharge trigger
is absorbed by receiver-side dedup on the event id.

### 4. Configuration

```toml
[webhooks]
url = "https://backend.example.com/lumen/events"
signing_key_env = "LUMEN_WEBHOOK_SECRET"
events = ["budget.threshold", "budget.exhausted", "key.disabled"]
thresholds = [50, 80, 95]        # percent, for budget.threshold
channel_capacity = 1024           # bounded; full = drop + counter
timeout_ms = 5000
max_attempts = 5
```

No `[webhooks]` block means no sender task, no outbound calls, no behavior
change. The block participates in hot reload like every other section
(ADR 008): an invalid block rejects the reload, a changed URL or event set
swaps atomically.

### 5. What this is not

- Not a generic eventing system: no per-request events, no usage streaming
  (the export route is for that), no fan-out to multiple receivers in v1
  (one URL; a backend can fan out itself).
- Not guaranteed delivery: at-least-once while the process lives, dropped
  with a counter when the receiver is down past the retry budget. The pull
  API is the system of record.
- Not a replacement for ADR 010's drift report: lifecycle events shrink the
  drift window; the console still diffs on its schedule.

## Consequences

- A billing backend can close the loop: threshold event, Stripe charge,
  `grant` top-up, all before the customer sees a 402. Today's alternative
  is polling `GET /admin/usage` on a tight interval.
- The gateway gains its first non-provider outbound call. The sovereignty
  stance is preserved by the opt-in default, but docs must state plainly
  that enabling webhooks makes the gateway call the configured URL.
- New metrics: `lumen_webhook_sent_total`, `lumen_webhook_dropped_total`,
  `lumen_webhook_dead_total`, delivery latency histogram.
- Testing needs a wiremock receiver: exactly-one signed event per crossing,
  5xx retry with backoff, overflow drop with counter, no secret in logs,
  clean cancellation on shutdown (issue #146 acceptance criteria).

## Future work

- Multiple receivers with per-receiver event filters.
- Quota events (`quota.rpm_exhausted`, `quota.tpm_exhausted`) once a
  sustained-rejection signal (as opposed to a single 429) is defined.
- A persistent outbox, only if reconciliation-by-pull proves insufficient
  in practice.
