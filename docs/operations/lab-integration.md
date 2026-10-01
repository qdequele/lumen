# Lab integration: account refs and billing usage events

The Meilisearch Lab is a control plane that manages accounts, keys and one
credit ledger across several products. LUMEN integrates with it through two
contracts only, never through Lab internals: the Lab **drives the admin API**
to create and top up keys, and LUMEN **pushes billing usage events** back so
the Lab can charge for what was spent. Nothing here sits on the request path,
and a gateway without the `[usage_events]` block behaves exactly as before.
The decision and its alternatives are in
[ADR 015](../adr/015-lab-integration.md).

## The lease model

- **One budget group per Lab account per gateway.** The group is the
  account's *lease*: its `budget_max` is the slice of the account's shared
  credit wallet this gateway may spend. An account that uses several gateways
  has one group on each, and each gateway reports its own events.
- **`account_ref`** on the group holds the Lab account id. **`external_ref`**
  on a key holds the Lab key id. Both are opaque text, 1 to 128 characters
  when set. Set them on `POST` and `PATCH`; `PATCH` with `null` clears one.
  Both are returned by the `GET` routes and filterable on the list routes:
  `GET /admin/groups?account_ref=...` and `GET /admin/keys?external_ref=...`.
- **`account_ref` must be a UUID while `[usage_events]` is configured**,
  because the Lab rejects any other account id. A violation is `400 LM-1001`
  naming the field. Without the block, any 1 to 128 character value is
  accepted.
- A key is **billable** when it has a group and that group has an
  `account_ref`. Every other key is an operator key: never billed, behaving
  exactly as today.

The Lab's loop, using routes that already exist:

1. `POST /admin/groups` with `account_ref` the first time the account enables
   LUMEN on this gateway.
2. `POST /admin/keys` with `group_id` and `external_ref` per customer key.
3. `POST /admin/groups/{id}/grant` to top the lease up, driven by the `group`
   snapshot carried in each usage event (see below). Admission is unchanged:
   an exhausted lease answers `402 LM-4001`, the correct answer for a customer
   who is out of credits. See [Keys, quotas & budgets](keys-budgets.md).

## Configuration

```toml
[usage_events]
url = "https://lab.internal"                 # events go to {url}/internal/events
signing_key_env = "LUMEN_USAGE_EVENTS_SECRET"
source = "eu-1"                              # gateway name copied into each event
batch_size = 500                             # 1..=1000
timeout_ms = 5000                            # 100..=60000
```

| Field | Required | Default | Bounds and meaning |
|---|---|---|---|
| `url` | yes | none | Control-plane base URL; a trailing slash is ignored. `https`, or plain `http` only to `localhost`, a loopback address (IPv4 or IPv6) or a private IPv4 address. |
| `signing_key_env` | yes | none | Name of the environment variable holding the HMAC signing secret, never the secret itself. Non-blank, no surrounding whitespace. |
| `source` | yes | none | Gateway name copied into every event. 1 to 64 characters of `A-Z a-z 0-9 . _ -`. |
| `batch_size` | no | `500` | Events per delivery request, 1 to 1000. |
| `timeout_ms` | no | `5000` | Per-request timeout, 100 to 60000 ms. |

- **Opt-in and restart-only.** The block is part of the boot layer
  ([Config source modes](config-modes.md)): a reload that changes it is
  refused, like any boot-layer key.
- **Requires `[auth] enabled = true`.** Otherwise the gateway refuses to boot.
- **The secret must be set.** The gateway refuses to boot when the variable
  named by `signing_key_env` is unset or empty. The error names only the
  variable. The secret is never logged, never in an error and never in
  `Debug` output.
- With no block there are no outbox rows, no sender task, no outbound call and
  no `lumen_usage_events_*` metric.

## The billing rule

Billing rides on budget settlement, so it is exactly what the budget
enforced.

- **Settled cost, not spend.** Each key carries a settled-cost counter, raised
  in `settle` by the real cost of a finished request. Reservation estimates
  held by in-flight requests are never billed.
- **Watermark.** Each key also carries a billing watermark: the part of its
  settled cost already turned into events, persisted in
  `virtual_keys.billed_micro`. At every budget flush, a billable key's event
  is the settled cost since the watermark. The delta is never negative, and a
  flush with nothing new settled emits nothing.
- **Atomic with the spend.** The event row is written in the same SQLite
  transaction that persists the key's spend and moves the watermark. After a
  failed commit nothing changes in memory and the next flush retries with a
  larger delta.
- **Non-billable keys never bill their past.** A key with no group, or in a
  group without `account_ref`, has its watermark kept equal to its settled
  cost. If it later joins a Lab-linked group, or its group gains an
  `account_ref`, only spend after that point is billed. Migration 0011
  back-fills every existing key's watermark from its current spend, so
  enabling `[usage_events]` on a running gateway bills only new spend.
- **Money is integer micro-USD.** Events carry `cost_micro_usd`; the bill is
  never computed from the floating-point `budget_spent` column. The Lab
  converts to credits.
- **One flush, one lock.** The periodic flush, the shutdown flush and the
  final flush of a deleted key share one function and one async lock, because
  two overlapping flushes would bill the same delta twice.
- **Deleted keys are retired, not dropped.** A deleted key stays in every
  flush until a flush that includes it has committed and no in-flight request
  still holds it. Its last spend, even from a request that is still
  streaming, is billed, and a failed final flush is retried.
- **Crash window: one flush.** A crash loses at most one flush interval of
  settled cost, from the bill and from the persisted budget alike
  ([ADR 009](../adr/009-shared-parent-budgets.md) section 4). The interval is
  `[auth] flush_interval_ms` (default 10000 ms).

### Changing billability flushes first, and fails closed

With `[usage_events]` configured, these two admin calls first flush pending
spend, so it is billed to the account in force when it was spent:

- `PATCH /admin/keys/{id}` with `group_id` in the body (including `null`);
- `PATCH /admin/groups/{id}` with `account_ref` in the body (including `null`).

If that flush fails, the call answers `500 LM-5001` and changes nothing.
Retry it. The consequence for operators: **while billing is on, a database or
outbox outage blocks key moves and `account_ref` changes.** Serving requests
and top-ups are unaffected.

Residual: spend from a request admitted before the change but settled after
it follows the new membership. Clear a group's `account_ref` only after the
account is closed.

## The event

One `usage.recorded` event per billable key per flush that has new settled
cost:

```json
{
  "id": "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61",
  "type": "usage.recorded",
  "occurred_at": "2026-09-30T15:04:05.123Z",
  "account_id": "<group.account_ref>",
  "api_key_id": "<key.external_ref or null>",
  "product": "lumen",
  "data": {
    "cost_micro_usd": 1834,
    "requests": 412,
    "tokens": 96120,
    "window": { "start": "2026-09-30T15:03:55.120Z", "end": "2026-09-30T15:04:05.123Z" },
    "source": "eu-1",
    "key_id": "<lumen key id>",
    "group": { "id": "<group id>", "spent_micro": 8123400, "budget_max_micro": 10000000 }
  }
}
```

- `id` is a UUIDv7 minted once, inside the transaction, so every retry carries
  the same id and the Lab can deduplicate on it.
- `occurred_at` and `window.end` are the flush time. `window.start` is the
  previous flush that billed this key (for its first event, the moment the
  key was loaded into memory at boot or created).
- `requests` and `tokens` are informational unit counts. A crash can lose
  them; money is only ever computed from the watermark.
- `group` is a snapshot of the lease at flush time, so the Lab can decide a
  top-up from the events alone. `budget_max_micro` is `null` for an unlimited
  group.
- The schema is vendored at `contracts/lab-events.schema.json`, and the
  serializer is tested against it.

## Delivery

One sender task per process, only when `[usage_events]` is set.

- Every 2 seconds (and immediately again while a full batch was just
  acknowledged), it selects up to `batch_size` pending rows whose retry time
  has come, oldest first, and sends
  `POST {url}/internal/events` with body `{"events":[...]}`.
- Headers: `Content-Type: application/json` and
  `X-Lab-Signature: sha256=<hex>`, where `<hex>` is the HMAC-SHA256 of the
  exact request body under the signing secret.
- A `200` response carries `{"accepted":["<id>", ...]}`. **Only ids listed in
  `accepted` are marked delivered.** Ids not listed stay pending.
- Anything else (connect error, timeout, `401`, any other non-success status,
  a body that is not the expected JSON) leaves every row of the batch
  pending. Each failed row waits `min(2^attempts, 300)` seconds before its
  next try, starting at 2 seconds and doubling, **capped at 5 minutes**, plus
  up to 1 second of jitter.
- A `401` is logged as an error and counted, and retried forever. A wrong
  secret never takes the gateway down.
- **Rows are never dropped.** This deliberately differs from
  [webhooks](keys-budgets.md#outbound-webhooks-for-budget-events), which are
  signals: these events are the bill. Delivered rows are purged after
  **7 days**, checked hourly.
- **Redirects are not followed**: the signed body reaches the configured
  endpoint only. A 3xx counts as a failed delivery with reason `status`.
- **Shutdown:** the final budget flush writes its events, then the sender
  gets one delivery attempt bounded to 5 seconds. Whatever is left is durable
  and goes out after the next boot.
- All SQLite access for delivery runs on the sender task, never on the
  request path.

## Metrics and the alert

| Metric | Meaning |
|---|---|
| `lumen_usage_events_pending` | Events in the outbox not yet acknowledged. |
| `lumen_usage_events_oldest_pending_seconds` | Age of the oldest unacknowledged event (0 when none). |
| `lumen_usage_events_delivered_total` | Events acknowledged by the control plane. |
| `lumen_usage_events_failed_total{reason}` | Events whose delivery attempt failed, one increment per event. `reason` is `connect`, `timeout`, `auth`, `status`, `malformed`, `not_accepted` or `store`. |

`not_accepted` means the Lab answered `200` but left that id out of
`accepted`; `store` means the outbox itself could not be read or updated.

The starter alert `LumenUsageEventsStuck` in
[`monitoring/prometheus/alerts.yml`](https://github.com/qdequele/lumen/blob/main/monitoring/prometheus/alerts.yml)
fires when `lumen_usage_events_oldest_pending_seconds > 900` for 5 minutes.
Serving is unaffected while it fires, but every bill is delayed. Look at
`lumen_usage_events_failed_total` by `reason`: `auth` is a secret mismatch,
`connect` and `timeout` are reachability, `status` is a Lab-side error, and
`not_accepted` is the Lab skipping events.

## Sovereignty

Enabling this block makes the gateway call the configured URL. Events carry
no prompt, no response, no request metadata and no model id: only the account
and key refs, counts, micro-USD cost, the lease snapshot and the gateway
name. Per-model detail stays in `usage_log` and `GET /admin/usage/export`,
which remain best-effort and are not the billing source. Without the block,
LUMEN makes no call to anything but its providers.

## What the Lab must provide

LUMEN carries no Lab code. For the integration to work, the Lab side must:

1. Accept `product: "lumen"` and a `lumenUsageData` definition matching the
   event above in its events schema.
2. Accept the `X-Lab-Signature` header on `POST /internal/events` (keeping
   `X-Scrapix-Signature` as an alias for Scrapix).
3. Convert `cost_micro_usd` to credits at its own rate.
4. Hold one events secret per LUMEN gateway.
5. Run the lease loop: create the group on first use, and grant from the
   `group` snapshot in each event before `spent_micro` reaches
   `budget_max_micro`.

CI carries an advisory job, `lab-contract-drift`, that compares the vendored
schema with the Lab's published copy once the repository variable
`LAB_EVENTS_SCHEMA_URL` is set.
