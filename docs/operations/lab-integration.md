# Lab integration: account refs and billing usage events

The Meilisearch Lab is a control plane that manages accounts, keys and one
credit ledger across several products. LUMEN integrates with it through two
contracts only, never through Lab internals: the Lab **drives the admin API**
to create and top up keys, and LUMEN **pushes billing usage events** back so
the Lab can charge for what was spent. Nothing here sits on the request path,
and a gateway without the `[usage_events]` block (and without the `LAB_URL`
and `LAB_INSTANCE_ID` variables) behaves exactly as before. The wire shapes
follow the Lab's platform contract v2. The decision and its alternatives are
in [ADR 015](../adr/015-lab-integration.md) and its 2026-10-09 amendment.

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
3. Keep the lease in step with the account's credits through
   `GET /admin/groups/{id}` and `PATCH /admin/groups/{id}` (or `grant`), see
   "The lease sync" below. Admission is unchanged: an exhausted lease answers
   `402 LM-4001`, the correct answer for a customer who is out of credits.
   See [Keys, quotas & budgets](keys-budgets.md).
4. Scope every call it proxies for a user with `X-Lumen-Account-Ref`, see
   "One gateway, many accounts" below.

## Configuration

The Lab mints one id and one secret per reporting deployment (platform
contract v2, section 3.3). Three environment variables, the same names
Scrapix and glutony read, are all a gateway needs:

```bash
LAB_URL=https://lab.meilisearch.com      # events go to {LAB_URL}/internal/events
LAB_INSTANCE_ID=<uuid>                   # this gateway's instance id
LAB_INSTANCE_SECRET=<64 hex>             # shown once by the Lab; never logged
```

`LAB_URL` and `LAB_INSTANCE_ID` overlay the `[usage_events]` block (and
create it when the file has none); `LAB_INSTANCE_SECRET` is the default
`secret_env`. The block spells the same thing out in TOML:

```toml
[usage_events]
url = "https://lab.meilisearch.com"      # LAB_URL
instance_id = "<uuid>"                   # LAB_INSTANCE_ID
secret_env = "LAB_INSTANCE_SECRET"       # env var holding the instance secret
source = "eu-1"                          # optional label for the boot log
batch_size = 500                         # 1..=500
timeout_ms = 5000                        # 100..=60000
```

| Field | Required | Default | Bounds and meaning |
|---|---|---|---|
| `url` | yes (or `LAB_URL`) | none | The Lab's base URL; a trailing slash is ignored. `https`, or plain `http` only to `localhost`, a loopback address (IPv4 or IPv6) or a private IPv4 address. No user or password, no query, no fragment. `LAB_URL` wins over the file. |
| `instance_id` | yes (or `LAB_INSTANCE_ID`) | none | The instance UUID the Lab minted. `LAB_INSTANCE_ID` wins over the file. A value that is not a UUID refuses to boot naming the field. |
| `secret_env` | no | `LAB_INSTANCE_SECRET` | Name of the environment variable holding the instance secret, never the secret itself. Non-blank, no surrounding whitespace. |
| `signing_key_env` | no | none | Deprecated alias of `secret_env` (the ADR 015 name). When set it wins, and boot logs one warning naming `secret_env` as the replacement. |
| `source` | no | the instance id | Label in the boot log, 1 to 64 characters of `A-Z a-z 0-9 . _ -`. Not in events: the Lab knows the instance from `X-Lab-Instance-Id`. |
| `batch_size` | no | `500` | Events per delivery request, 1 to 500: the Lab answers a larger batch with a 400 for the whole batch, which would never be acknowledged, so a larger value refuses to boot. |
| `timeout_ms` | no | `5000` | Per-request timeout, 100 to 60000 ms. Also bounds the boot-time identity call. |

- **Opt-in and restart-only.** The block is part of the boot layer
  ([Config source modes](config-modes.md)): a reload that changes it is
  refused, like any boot-layer key.
- **Requires `[auth] enabled = true`.** Otherwise the gateway refuses to boot
  (`[usage_events] requires auth.enabled = true`). This includes a block
  created by the environment alone: a gateway with auth off that inherits
  `LAB_URL` from an env file shared with Scrapix or glutony refuses to boot,
  so keep those variables out of a standalone gateway's environment.
- **Both halves are needed.** `LAB_INSTANCE_ID` without any url refuses to
  boot with an error naming `LAB_URL`, and a url without an instance id
  refuses to boot naming `LAB_INSTANCE_ID`.
- **The secret must be set.** The gateway refuses to boot when the variable
  named by `secret_env` (or `signing_key_env`) is unset, empty or only
  whitespace. The error names only the variable. The secret is never logged,
  never in an error and never in `Debug` output.
- **Every live group's `account_ref` must already be a UUID.** The admin API
  only enforces the UUID shape while the block is on, so a group given some
  other ref before billing was enabled would produce events the Lab rejects.
  With the block configured, the gateway checks every live group at boot and
  refuses to start if one has a non-UUID `account_ref`; the error lists the
  offending group ids and nothing else. To fix it: boot without
  `[usage_events]` (and without `LAB_URL` / `LAB_INSTANCE_ID`),
  `PATCH /admin/groups/{id}` each listed group with its Lab account UUID (or
  `null` if it is not a Lab account), then restore the block and restart.
- With no block (and neither `LAB_URL` nor `LAB_INSTANCE_ID` set) there are
  no outbox rows, no sender task, no `lumen_usage_events_*` metric and no
  call to the Lab.
- **Removing the block strands pending events.** Rows still in the outbox
  stay there unsent while the block is absent (they go out once it is back).
  Drain first: wait for `lumen_usage_events_pending` to reach 0 (stop billable
  traffic if new events keep arriving), stop the gateway, confirm
  `SELECT COUNT(*) FROM usage_outbox WHERE delivered_ms IS NULL AND dropped_ms IS NULL`
  is 0 (the shutdown flush may have written a last event), then remove the
  block.

## Instance identity

At boot, with the block configured and before the sender starts, the
gateway calls `GET {LAB_URL}/internal/instances/me` once, with
`Authorization: Bearer <instance secret>` and `X-Lab-Instance-Id` (contract
v2, section 3.6), and logs the answer (`instance_id`, `region`, and the
`source` label). Engines are hosted by Meilisearch only (decision A of the
contract): the instance `kind` is `hosted`, and every event the gateway
reports is billed by the Lab.

- **Boot is refused** when the Lab answers `401` or `403` (the credentials
  are wrong: check `LAB_INSTANCE_ID` and the secret), or answers `2xx` with
  a body that is not this gateway's identity: a malformed body, an
  `instance_id` other than the configured one (compared case-insensitively),
  a `product` other than `lumen` (the Lab would skip every event with
  `product mismatch`) or a `kind` other than `hosted`.
- **Anything else only warns** (any other status, for example a `404` from a
  Lab that does not serve the endpoint yet or a `5xx`, a connect error, a
  timeout, or a `2xx` whose body stalls or breaks mid-read): the gateway
  logs one warning and boots. Events are durable and
  the sender retries them.
- **An unreachable Lab delays serving.** The call is bounded by
  `timeout_ms` (default 5000 ms) and runs after the listener is bound but
  before the gateway starts serving: while a Lab does not answer, the socket
  accepts TCP connections but HTTP requests, `/health` included, wait up to
  that long. HTTP readiness probes need that margin; TCP probes pass at once.
- Billing attribution is unchanged from ADR 015: a key is billed when its
  group has an `account_ref`; every other key is an operator key.

## One gateway, many accounts

A hosted gateway serves many Lab accounts through one master key, so the
gateway enforces the account boundary itself (contract v2, section 8.3).
The Lab sets `X-Lumen-Account-Ref: <account uuid>` on every `/admin/*` call
it proxies for a user. The UUID is matched in its canonical lowercase form
whatever case the caller sends, and a group created under the header stores
that lowercase form. With that header:

| Routes | Behaviour |
|---|---|
| `GET /admin/keys`, `GET /admin/groups`, `GET /admin/usage`, `GET /admin/usage/export` | Only rows of groups whose `account_ref` equals the header. A key with no group is never listed. A query filter naming another account (`?account_ref=` on groups, `?key_id=` or `?group_id=` on usage) matches nothing: a filter narrows the scope, never widens it. |
| `PATCH`, `DELETE`, `rotate` and `grant` on a key; `GET`, `PATCH`, `DELETE` and `grant` on a group: when the key or group belongs to another account, or the key has no group | `404 LM-1003`, the same envelope an unknown id gets, so a scoped caller cannot tell a foreign id from one that never existed. |
| `POST /admin/keys` | `group_id` is required and must name a group of the account (`400 LM-1001` without one, `404 LM-1003` for another account's group). A scoped `PATCH` cannot clear `group_id` (`400 LM-1001`) or move the key to another account's group (`404 LM-1003`). |
| `POST /admin/groups` | `account_ref` is set to the header's value when absent; a different value (compared ignoring case) is `400 LM-1001`. A scoped `PATCH` that carries `account_ref` at all is `400 LM-1001`. |
| `/admin/provider-keys/*`, `/admin/providers/{name}/check`, `/admin/webhooks*`, `/admin/config*`, `/openapi.json`, `/health/providers` | `403 LM-4005`: platform-only, whatever the header's value. `/health/providers` needs no key without the header, but it names the platform's providers and their health, so a scoped call never sees it. |

- **Webhooks are platform-only rather than filtered.** The budget webhook
  receiver is one gateway-wide setting that carries every account's budget
  events, so no account may read or change it. A per-account receiver is a
  possible later change.
- On the account routes, a header that is not exactly one UUID (malformed,
  or sent twice) is `400 LM-1001`, never read as "unscoped". The master key
  is checked before the scope, so a missing or wrong key is still
  `401 LM-4004`.
- **Send the canonical lowercase UUID.** The account compare is exact. An
  uppercase header fails closed (empty lists, `404` on every id), but a
  scoped `POST /admin/groups` would store it as sent.
- **Usage follows the group's current account.** The usage routes filter on
  the `account_ref` a group has now: if an operator moves a group to another
  account, its usage history moves with it.
- Without the header the master key keeps its unscoped, platform-operator
  behaviour, so an operator's scripts and the Lab's own provisioning calls
  are unchanged. The scope check and the write are separate store calls;
  only an unscoped master-key call running at the same moment can race them.

## The lease sync

The Lab keeps each account's lease in step with its credits (contract v2,
section 8.2) through three routes, all accepting the scoping header:

| Route | What the Lab does with it |
|---|---|
| `GET /admin/groups/{id}` | Reads the stored group record plus the live `spent_micro` (in-flight reservations included, like admission) and `budget_max_micro` (`null` when capless). |
| `PATCH /admin/groups/{id}` with `budget_max` | Sets the cap absolutely (USD), so that `budget_max_micro - spent_micro` equals the account's remaining credits in micro-USD. |
| `POST /admin/groups/{id}/grant` with `amount` | Raises the cap by `amount` USD atomically (two concurrent top-ups both land). |

An account at zero credits has a zero remaining lease and the gateway
refuses its keys itself (`402 LM-4001`).

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
- **Money is integer micro-USD.** Events carry `provider_cost_micro_usd`;
  the bill is never computed from the floating-point `budget_spent` column.
  The Lab prices it, with the unit counts, into credits.
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

A `PATCH /admin/keys/{id}` that changes only `external_ref` does not flush
first: it cannot change billability or the account. Spend settled before the
change but not yet flushed is reported under the new `api_key_id`, on the
same account, with no effect on the money billed.

## The event

One `usage.recorded` event per billable key per flush that has new settled
cost, in the Lab-owned v2 shape (`contracts/vendor/lab/lab-events.schema.json`):

```json
{
  "id": "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61",
  "type": "usage.recorded",
  "occurred_at": "2026-09-30T15:04:05.123Z",
  "account_id": "<group.account_ref>",
  "api_key_id": "<key.external_ref or null>",
  "product": "lumen",
  "data": {
    "operation": "gateway",
    "units": { "requests": 412, "tokens_in": 90120, "tokens_out": 6000, "tokens_estimated": 310 },
    "provider_cost_micro_usd": 1834,
    "description": "key <lumen key id> 2026-09-30T15:03:55.120Z..2026-09-30T15:04:05.123Z"
  }
}
```

- `id` is a UUIDv7 minted once, when the outbox row is built just before the
  flush transaction writes it, so every retry carries the same id and the
  Lab deduplicates on it.
- `operation` is always `gateway`: one event aggregates every capability of
  the key over the window, because LUMEN keeps one watermark per key. (The
  Lab schema's enum for `lumen` also lists `chat`, `embed`, `rerank` and
  `systemone`; that list predates the `decisions` rename of ADR 017, and
  LUMEN sends `gateway` only.)
- `units` are raw counts the Lab prices: settled requests, input and output
  tokens, and `tokens_estimated`, the tokens (in plus out) of the requests
  whose counts were local estimates (ADR 003). A crash can lose units; money
  is only ever computed from the watermark.
- `provider_cost_micro_usd` is the settled cost since the watermark, in
  integer micro-USD, passed through and marked up by the Lab. Never the
  floating-point `budget_spent` column, never a reservation.
- `description` is `key <lumen key id> <window start>..<window end>`.
  `occurred_at` and the window end are the flush time; the window start is
  the previous flush that billed this key (for its first event, the moment
  the key was loaded into memory at boot or created).
- The instance is not in the body: the Lab takes it from the authenticated
  `X-Lab-Instance-Id` header. The lease snapshot (`group`), `source`,
  `key_id` and `window` that ADR 015 carried in `data` are gone.
- The schema's `usageData` forbids additional properties, so nothing else
  is sent. The schema is the Lab's, vendored byte for byte and never edited
  here; the serializer is tested against the vendored copy. The CI job
  `lab-contract-drift` compares it with `meilisearch/lab` main and fails on
  drift when the `LAB_REPO_TOKEN` secret is set; without the secret it skips
  with a notice, and while the Lab has not published its copy yet (`404`) it
  passes with a warning.

## Delivery

One sender task per process, only when `[usage_events]` is set.

- Every 2 seconds (and immediately again while a full batch was just
  acknowledged), it selects up to `batch_size` pending rows whose retry time
  has come, oldest first, and sends
  `POST {url}/internal/events` with body `{"events":[...]}`.
- Headers: `Content-Type: application/json`, `X-Lab-Instance-Id:
  <LAB_INSTANCE_ID>`, `X-Lab-Timestamp: <unix seconds>` and
  `X-Lab-Signature: sha256=<hex>`, where `<hex>` is the HMAC-SHA256, under
  the instance secret, of `<X-Lab-Timestamp>.<raw body>`. The timestamp is
  taken fresh for every attempt. The Lab rejects a timestamp more than 300 s
  from its clock: keep the gateway's clock synced.
- A `2xx` response carries `{"accepted":["<id>", ...]}`. **Only ids listed in
  `accepted` are marked delivered.** Ids not listed stay pending (see the
  drop rule below).
- Anything else (connect error, timeout, `401`, any other non-2xx status,
  a body that is not the expected JSON) leaves every row of the batch
  pending. Each failed row waits `min(2^attempts, 300)` seconds before its
  next try, starting at 2 seconds and doubling, **capped at 5 minutes**, plus
  up to 1 second of jitter.
- A `401` is logged as an error (check `LAB_INSTANCE_ID` and the secret),
  counted, and retried forever. A wrong secret never takes the gateway down.
- **Rows are dropped in exactly one case.** An event a reachable Lab keeps
  out of `accepted` is permanently rejected after 24 h (contract v2,
  section 3.5). The clock starts at the first `2xx` answer that leaves the
  event out (`usage_outbox.first_skipped_ms`, set once and never reset); the
  next such answer at least 24 h later drops it: the sender sets
  `usage_outbox.dropped_ms`, logs an error naming the event id and
  increments `lumen_usage_events_dropped_total`. The body stays in the table
  until the 7-day purge, so the amount can still be reconciled by hand. A
  Lab that is unreachable, times out or answers a non-2xx status never
  starts or resets the clock and never drops anything, however long it
  lasts: those rows wait. This deliberately differs from
  [webhooks](keys-budgets.md#outbound-webhooks-for-budget-events), which are
  signals: these events are the bill. Delivered and dropped rows are purged
  after **7 days**, checked hourly.
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
| `lumen_usage_events_pending` | Events in the outbox neither acknowledged nor dropped. |
| `lumen_usage_events_oldest_pending_seconds` | Age of the oldest pending event (0 when none). |
| `lumen_usage_events_delivered_total` | Events acknowledged by the control plane. |
| `lumen_usage_events_failed_total{reason}` | Events whose delivery attempt failed, one increment per event. `reason` is `connect`, `timeout`, `auth`, `status`, `malformed`, `not_accepted` or `store`. |
| `lumen_usage_events_dropped_total` | Events the Lab kept refusing for 24 h and the gateway dropped. |

`not_accepted` means the Lab answered `2xx` but left that id out of
`accepted`, and counts only rows that will be retried (a dropped row counts
in `lumen_usage_events_dropped_total` alone); `store` means the outbox itself
could not be read or updated. A rising `lumen_usage_events_dropped_total`
means the Lab refuses these events for good (an account this instance may
not report for, an unknown account): read the error log for the ids.

The starter alert `LumenUsageEventsStuck` in
[`monitoring/prometheus/alerts.yml`](https://github.com/qdequele/lumen/blob/main/monitoring/prometheus/alerts.yml)
fires when `lumen_usage_events_oldest_pending_seconds > 900` for 5 minutes.
Serving is unaffected while it fires, but every bill is delayed. Look at
`lumen_usage_events_failed_total` by `reason`: `auth` is a secret mismatch,
`connect` and `timeout` are reachability, `status` is a Lab-side error, and
`not_accepted` is the Lab skipping events. A second starter alert,
`LumenUsageEventsDropped`, fires on
`increase(lumen_usage_events_dropped_total[1h]) > 0`: an event was dropped
under the 24 h rule and was never billed; follow the runbook below to
reconcile it.

### Runbook: an event the Lab never accepts

If delivery works for new events but the alert stays on, one row is probably
being refused (typically `not_accepted` keeps rising by the same small
count), and it pins `lumen_usage_events_oldest_pending_seconds`. Left alone,
it is dropped 24 h after the first refusal (see the drop rule in
"Delivery") and the alert clears; the steps below settle it sooner or
reconcile it afterwards. The outbox is the `usage_outbox` table of the auth
database (`[auth] db_path`). Inspect the oldest pending rows read-only:

```bash
sqlite3 -readonly lumen.db "SELECT id, attempts, created_ms, next_attempt_ms, first_skipped_ms, body \
  FROM usage_outbox WHERE delivered_ms IS NULL AND dropped_ms IS NULL ORDER BY created_ms, id LIMIT 5"
```

A non-null `first_skipped_ms` is when the Lab first answered without the id
(unix ms): the row is dropped at the first refusal 24 h after it. List what
was already dropped (and is kept until the 7-day purge) with:

```bash
sqlite3 -readonly lumen.db "SELECT id, first_skipped_ms, dropped_ms, body \
  FROM usage_outbox WHERE dropped_ms IS NOT NULL"
```

Look at `body` (the exact event sent) and at the Lab's logs for that `id` to
learn why it is refused. Then either:

- **fix the cause on the Lab side** (an account it does not know yet, an
  instance not allowed to report for it) within the 24 h: the row is
  retried unchanged and clears on its own; or
- **settle it by hand** when the Lab must never take it, for example because
  the body itself is wrong (a row's body never changes once written):
  reconcile the amount with the Lab first (charge or waive it there), then
  mark that one row delivered, so it is purged after 7 days like any other:

```bash
sqlite3 lumen.db "UPDATE usage_outbox SET delivered_ms = CAST(strftime('%s','now') AS INTEGER) * 1000 \
  WHERE id = '<event id>' AND delivered_ms IS NULL AND dropped_ms IS NULL"
```

A dropped row was never billed: reconcile its amount with the Lab by hand
before the 7-day purge removes it. Never delete a pending row: it is the
only record of that bill.

### Upgrading a build from main with pending v1 events

No tagged release shipped the first usage-event shape (v1). Builds from
`main` since the first ADR 015 usage-events commit, after 0.6.1, may still
hold pending outbox rows in that shape, and a v2 Lab cannot validate them.
A v1 body has no `data.operation`. Those builds also predate the
`dropped_ms` column (migration 0013 adds it when the new build boots), so
set the rows aside in two steps.

1. Stop the old gateway, then inspect the v1 rows still pending and keep
   the output, it is the only record of that spend:

   ```bash
   sqlite3 -readonly lumen.db "SELECT id, attempts, created_ms, body FROM usage_outbox \
     WHERE delivered_ms IS NULL AND json_extract(body, '$.data.operation') IS NULL \
     ORDER BY created_ms, id"
   ```

2. Still before upgrading, park them so the new sender never selects them:

   ```bash
   sqlite3 lumen.db "UPDATE usage_outbox SET next_attempt_ms = 9223372036854775807 \
     WHERE delivered_ms IS NULL AND json_extract(body, '$.data.operation') IS NULL"
   ```

3. Upgrade and start the new build, then mark the parked rows dropped (now
   in milliseconds), so they leave `lumen_usage_events_pending` and are
   purged after 7 days like any dropped row:

   ```bash
   sqlite3 lumen.db "UPDATE usage_outbox SET dropped_ms = CAST(strftime('%s','now') AS INTEGER) * 1000 \
     WHERE delivered_ms IS NULL AND dropped_ms IS NULL AND json_extract(body, '$.data.operation') IS NULL"
   ```

The gateway never sends these rows, so the spend they carry
(`data.cost_micro_usd` in each body) must be reconciled with the Lab by
hand.

## Sovereignty

Enabling this block makes the gateway call the configured URL. Events carry
no prompt, no response, no request metadata and no model id: only the account
and key refs, the LUMEN key id, unit counts, micro-USD cost and the window.
The boot-time identity call sends the instance credentials (the id and the
bearer secret) and nothing else. Per-model detail stays in `usage_log` and
`GET /admin/usage/export`, which remain best-effort and are not the billing
source. Without the block, this block adds no outbound call (webhooks, if
configured, are separate).

## What the Lab must provide

LUMEN carries no Lab code. The Lab side of platform contract v2 is:

1. Mint the instance id and secret (the hosted-engine rake task,
   `bin/rails lab:hosted_engine:create PRODUCT=lumen ...`) and serve
   `GET /internal/instances/me`.
2. Accept `product: "lumen"` with `operation: "gateway"` and the four unit
   names in its own `contracts/lab-events.schema.json` (the file LUMEN
   vendors), and price them in `saas/config/pricing.yml` (LUMEN's money is
   `provider_cost_micro_usd`).
3. Verify `X-Lab-Instance-Id`, `X-Lab-Timestamp` and `X-Lab-Signature` on
   `POST /internal/events` and answer `{"accepted": [...]}`.
4. Validate each event of a batch on its own and leave an invalid one's id
   out of `accepted`; never answer non-2xx for a whole batch because of one
   event. A non-2xx never starts the gateway's 24 h drop clock, so one bad
   event would stall billing for every event behind it.
5. Set `X-Lumen-Account-Ref` (the canonical lowercase account UUID) on every
   proxied `/admin/*` call and never expose the platform-only routes to
   users.
6. Run `SyncLumenLeasesJob` against `GET /admin/groups/{id}` and
   `PATCH /admin/groups/{id}` (or `grant`), see "The lease sync".
