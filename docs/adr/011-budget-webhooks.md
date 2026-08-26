# ADR 011 - Outbound webhooks for budget events

- Status: accepted
- Date: 2026-08-25 (amended 2026-08-26: admin control surface)
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

## Amendment 2026-08-26: the webhook configuration is an admin resource

§4 above made `[webhooks]` a config-file section, reloadable but with three
knobs that could only change across a restart (`channel_capacity`,
`signing_key_env`, and the presence of the block itself). That is the wrong
shape for the very caller this feature exists for: a billing control plane
provisions a gateway through the admin API, and cannot restart it or edit its
environment. The section stays, and it gains a control surface.

### 1. Routes

Master-key gated, alongside the other `/admin` routes:

- `GET /admin/webhooks` - the live settings, secret-free. Reports which
  source they came from and whether deliveries are signed.
- `PUT /admin/webhooks` - replace **every** setting; applied immediately and
  persisted, so a restart comes up identically.
- `DELETE /admin/webhooks` - stop emitting, persisted.
- `PUT /admin/webhooks/signing-key` - store the HMAC secret, sealed at rest.

There is still exactly **one** receiver (§5 stands): these routes edit that
receiver, they do not create a collection. Fan-out remains future work.

### 2. Precedence: a stored row wins over the file

Settings written through `PUT` land in a single-row `webhook_config` table in
the auth database. Resolution at boot and on every reload:

1. A stored row with `enabled = 1` wins outright.
2. A stored row with `enabled = 0` means off, whatever the file says - so
   `DELETE` is not silently undone by the next reload.
3. No row at all falls back to the `[webhooks]` file block (or to off).

The file therefore stays the declarative default for a GitOps deployment that
never calls the API, and the API wins for a control-plane deployment. This is
the provider-key rule (ADR 008) with the sources swapped: there, config-named
environment variables are primary and the database fills in; here the database
is an explicit operator override of a declarative default. The asymmetry is
deliberate - a `PUT` that the next reload reverted would be a bug, whereas a
provider key that the environment overrode is the documented contract.

`GET` names the source (`"database"` / `"config"` / `"none"`) so drift between
the file and the live setting is visible rather than inferred, in the spirit of
ADR 010's drift report.

### 3. Every field is editable at runtime

- `url`, `events`, `thresholds`, `timeout_ms`, `max_attempts`,
  `retry_base_ms`: swapped in the live policy cell, as before.
- `channel_capacity`: the bounded queue is **rebuilt**. The new queue takes
  new events; the previous sender task keeps its receiver, drains whatever was
  already queued under the settings it had, and exits when its last sender is
  dropped. Nothing already accepted is discarded to change a capacity.
- The signing secret: held in a swappable cell the sender reads per attempt,
  so a rotation applies to the next delivery without restarting the task.

Enabling webhooks on a process that booted without them therefore works too:
the queue, the sender task and the Prometheus collectors are created on the
**first** enable, not at boot. A gateway that never enables webhooks exports no
`lumen_webhook_*` series at all, which keeps the opt-in default honest on
`/metrics` as well as on the wire.

### 4. The secret, and why it may cross the API

§3 above read the secret only from an environment variable named in config.
That is still the primary source, and still the only one for an operator who
manages secrets through their deployment system. But a control plane that
provisions a gateway it does not own the environment of needs a way in, so
`PUT /admin/webhooks/signing-key` accepts the secret in the body and seals it
with AES-256-GCM under the master key - byte for byte the
`PUT /admin/provider-keys/{name}` mechanism (ADR 008), including its threat
model: the database file and `LUMEN_MASTER_KEY` together decrypt it, either
alone does not.

Resolution order, evaluated at boot, on reload, and on every apply:

1. The variable named by `signing_key_env`, when set and non-empty.
2. The stored secret.
3. Neither: deliveries are unsigned. Refused as an error when
   `signing_key_env` was named (that is a broken deployment, not a choice) and
   allowed with a loud warning when it was deliberately omitted.

No route ever returns the secret, in any form. `GET /admin/webhooks` reports a
boolean `signed` and the variable *name*, exactly as the provider surface
reports key presence without key material.

### 5. What this does not change

Detection, edge-triggering, payload construction, at-least-once delivery,
idempotency and the "never on the request path" guarantee are all untouched.
The admin surface configures the pipeline; it is not part of it.

## Future work

- Multiple receivers with per-receiver event filters.
- Quota events (`quota.rpm_exhausted`, `quota.tpm_exhausted`) once a
  sustained-rejection signal (as opposed to a single 429) is defined.
- A persistent outbox, only if reconciliation-by-pull proves insufficient
  in practice.
