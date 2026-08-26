# Keys, quotas & budgets

Auth is **off by default**: with `[auth].enabled = false` the gateway is an
open proxy, with no database at all. Turn it on to get virtual keys, hard
budgets and RPM/TPM quotas.

## Enabling

```toml
[auth]
enabled = true
db_path = "lumen.db"   # SQLite file, created if missing
```

When enabled, the `LUMEN_MASTER_KEY` environment variable is **required**:
64 hex characters (32 bytes). It serves two roles - the bearer token of the
`/admin/*` API, and the AES-256-GCM key that seals provider keys stored at
rest. It is never logged and never stored.

## What you get

With auth on, every `/v1/*` request is checked against a **virtual key**:

- **Virtual keys** are stored only as BLAKE3 hashes; the plaintext is
  returned exactly once, at creation, and never again.
- **Hard budgets** (`budget_max`, in USD) and **RPM/TPM quotas**
  (`rpm_limit`, `tpm_limit`) are enforced **in memory, before any upstream
  call** - a rejected request never spends and never reaches a provider.
- The database is **never on the request path**. Budget spend is flushed
  from memory to SQLite on `flush_interval_ms` (default 10000). While the
  process is running, enforcement itself lives in memory ahead of the
  flush, so budget overruns cannot occur. A crash loses at most that much
  *accounting*; after a restart, budgets reload from the last persisted
  state, so unflushed usage lost in a crash can permit spend beyond the
  intended budget until the gap closes.
- That database is the **only copy** of the key hashes, stored provider
  keys, budget state and usage ledger: see
  [Backups](deployment.md#backups) for how not to lose it, and
  [Scaling and high availability](deployment.md#scaling-and-high-availability)
  for why in-memory enforcement means one instance in v1.

## Refusals

| Code | HTTP | Cause |
|---|---|---|
| `LM-4001` | 402 | Hard budget exhausted - the key's own budget or its [budget group](#budget-groups)'s shared pool. Same code for both; the message text says which scope refused. |
| `LM-4002` | 429 | Requests-per-minute quota exceeded. |
| `LM-4003` | 429 | Tokens-per-minute quota exceeded. |
| `LM-4004` | 401 | Missing or invalid virtual key - deliberately unspecific: unknown, disabled and expired keys are indistinguishable, so a caller cannot probe key state. |

See [Error codes](../errors.md) for the full taxonomy.

## Bootstrapping the first key (CLI)

On a fresh deployment there is no usable client key yet, and the admin API
requires a running server. `lumen keys` closes that loop: it runs **offline**,
straight against the SQLite file at `auth.db_path` - no server needed.

```bash
export LUMEN_MASTER_KEY=<64 hex chars>   # same gate as the /admin API
lumen keys create --config config.toml --name team-search \
  --budget-max 50 --rpm-limit 60 --tpm-limit 100000
lumen keys list --config config.toml
```

`keys create` prints the record plus the **one-time plaintext key** as a JSON
object on stdout (the same shape as `POST /admin/keys`, shown below) and never
logs it. `--budget-max`, `--rpm-limit`, `--tpm-limit` and `--expires-at` are
optional, exactly like their JSON counterparts; `keys list` prints the records
only (no hashes, no plaintext).

If the server is already running, `keys list` is safe, but a key created by
the CLI only joins the live in-memory key table at the next restart or config
reload - prefer `POST /admin/keys` against the running server in that case.

## The admin API

Every route under `/admin/*` is mounted only when `[auth].enabled = true`,
and every route is gated by the master key (`Authorization: Bearer
<LUMEN_MASTER_KEY>`). Changes apply to the database and the in-memory state
together, so they take effect immediately with no restart.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/admin/keys` | Create a virtual key. |
| `GET` | `/admin/keys` | List active keys (records only - no hashes, no plaintext). `?include_deleted=true` adds tombstones. |
| `PATCH` | `/admin/keys/{id}` | Adjust budget/limits, or enable/disable a key. |
| `DELETE` | `/admin/keys/{id}` | Soft-delete a key (tombstone; stops authenticating immediately). |
| `POST` | `/admin/keys/{id}/rotate` | Mint a new secret for an existing key (one-time plaintext, identity and spend preserved). |
| `POST` | `/admin/keys/{id}/grant` | Atomically add to a key's budget cap (concurrency-safe top-up). |
| `POST` | `/admin/groups` | Create a budget group - a shared pool member keys draw from. |
| `GET` | `/admin/groups` | List active groups. `?include_deleted=true` adds tombstones. |
| `PATCH` | `/admin/groups/{id}` | Adjust a group's name or shared budget (pool spend preserved). |
| `DELETE` | `/admin/groups/{id}` | Soft-delete a group. Refused while it still has active member keys. |
| `POST` | `/admin/groups/{id}/grant` | Atomically add to a group's shared budget cap (concurrency-safe top-up). |
| `PUT` | `/admin/provider-keys/{name}` | Store a provider API key encrypted at rest. |
| `GET` | `/admin/usage` | Aggregated usage and spend from the usage log. |

### Budget groups

A **budget group** is a shared pool that several keys draw from
([ADR 009](../adr/009-shared-parent-budgets.md)). The pattern it exists
for: prepaid credits per customer, one key per project of that customer.
Without groups you would chunk the customer's credit across the project
keys and rebalance from outside - racy, and credit strands on idle keys.
With groups: one group per customer, its keys join it, and a top-up is a
single [grant](#granting-budget-top-ups).

Create the pool first:

```bash
curl -s http://localhost:8080/admin/groups \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "acme-corp", "budget_max": 500.0}'
```

`name` is required; `budget_max` (USD) is optional - omit it for an
unlimited pool that only attributes usage. A group has no plaintext and no
hash, so unlike key creation there is **no one-time secret**: the `201`
response is just the record, and `GET /admin/groups` lists the same shape.

```json
{
  "id": "...",
  "name": "acme-corp",
  "budget_max": 500.0,
  "budget_spent": 0.0,
  "created_at": 1752537600,
  "deleted_at": null
}
```

A group carries **budget only**: no group-level RPM/TPM, no `disabled`
flag, no expiry (each member key's own limits still apply; group-level
limits are backlog).

**Membership.** A key belongs to **at most one group**, via `group_id` on
`POST /admin/keys` or `PATCH /admin/keys/{id}`:

```bash
curl -s http://localhost:8080/admin/keys \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "acme-project-search", "group_id": "<group id>", "budget_max": 100.0}'
```

On the patch, `group_id` is **tri-state**: leave the field out and
membership is unchanged, send JSON `null` and the key leaves its group,
send a string and it joins that group. This is the one deliberate
exception to "patch fields cannot clear to NULL" - leaving a group must
not require re-minting the key. A `group_id` naming an unknown or deleted
group is refused with 400 `LM-1001` before anything is written. Key
records expose `group_id` wherever records appear. `lumen keys create`
takes `--group-id <ID>` too, but the group must already exist: there is no
offline `lumen groups` subcommand - create groups through the admin API,
which needs only the master key.

**Enforcement** works exactly like a key budget, one level up:

- **Admission checks both.** The key's own `budget_max` (when set) AND the
  group pool, in memory, before any upstream call. Either refusal is a
  402 `LM-4001`; the message says which scope refused ("budget exceeded
  for this key" vs "budget exceeded for this key's group").
- **Refunds mirror per-key budgets**
  ([ADR 007](../adr/007-accounting-refinements.md)): a request refused at
  admission consumes nothing from either pool, and actual cost settles
  against both on completion.
- **Flush and crash semantics are identical** to key budgets: pool spend
  flushes to SQLite on `flush_interval_ms`, enforcement lives in memory
  ahead of the flush, and a crash loses at most one flush interval of pool
  *accounting*.

Pool spend is the group's own accumulator, never recomputed from its
members: spend a key accrued before joining (or after leaving) is not
moved retroactively.

To top up a customer, raise the cap - pool spend is preserved, and the new
cap binds every member key on its next request, no restart (`name` can be
patched the same way; an unknown or deleted id returns 400 `LM-1001`):

```bash
curl -s -X PATCH http://localhost:8080/admin/groups/<id> \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"budget_max": 1000.0}'
```

A `PATCH` sends an **absolute** cap, so top-ups that can race (an
automated billing flow) should use
[grant](#granting-budget-top-ups) instead - it adds atomically and never
loses a concurrent update.

`DELETE /admin/groups/{id}` is a **soft delete**, like keys, and it is
refused (400) while the group still has active member keys: move them out
(`PATCH` with `"group_id": null` or another group) or delete them first.
Silently dropping members out of pool enforcement would be worse than the
error. The tombstone keeps `usage_log` attribution, and its final pool
spend is flushed on removal.

**Attribution.** Every `usage_log` row - successes and 402/429 refusal
rows alike - carries the key's group id (when it has one), so per-customer
reporting includes the traffic the pool refused. `GET /admin/usage`
(below) takes a `group_id` filter and `group_by=group_id`; under that
grouping, rows from ungrouped keys aggregate under an empty group name.

### Granting budget (top-ups)

`POST /admin/groups/{id}/grant` adds to a pool's cap **atomically**. It
exists for the prepaid-credits flow above: a customer buys credits, and
the billing control plane grants their pool.

```bash
curl -s -X POST http://localhost:8080/admin/groups/<id>/grant \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"amount": 25.0}'
```

The body is `{"amount": <USD>}`, the amount to **add** to `budget_max`;
spend and quota windows are untouched. The `200` response is the updated
group record, the same shape as `GET /admin/groups`. The key half is
`POST /admin/keys/{id}/grant`: same body, same rules, and the `200`
response is the updated key record (the record only - no plaintext,
nothing was minted).

Why not a `PATCH`? A patch sends an **absolute** cap, so two racing
top-ups can lose one: both workers read 500, both add 25, both write 525,
and one customer payment vanishes. A grant is an atomic increment on
**both** sides - `budget_max = budget_max + ?` inside SQLite, a
`fetch_add` on the live in-memory entry - so concurrent grants all land.
Like every admin change it takes effect on the very next request, no
restart.

Three refusals, all `400` `LM-1001`:

- **A non-positive, non-finite or oversized `amount`.** Zero and negatives
  are rejected; overflowing literals like `1e999` never even parse; and a
  single grant is capped at `1e12` USD so repeated grants can never sum
  the stored cap toward infinity (which would read back as *unlimited*).
- **Unknown or deleted id**, like every other admin write.
- **A capless target** (`budget_max` null): there is no cap to raise, and
  silently doing nothing would be worse than the error. Set a cap first
  (`PATCH {"budget_max": ...}`), then grant.

One retry caveat for billing automation: a grant is **not idempotent**. A
call that timed out or disconnected may still have landed in the
database, and blindly retrying would credit the cap twice - verify with
`GET /admin/groups` (or `/admin/keys`) before retrying.

### Create a key

```bash
curl -s http://localhost:8080/admin/keys \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "name": "team-search",
    "budget_max": 50.0,
    "rpm_limit": 60,
    "tpm_limit": 100000
  }'
```

`name` is required; `budget_max`, `rpm_limit`, `tpm_limit` and `expires_at`
(unix seconds) are all optional - omit any of them for "unlimited". The
response is the **only place the plaintext key ever appears**:

```json
{
  "key": "sk-lumen-...",
  "id": "...",
  "name": "team-search",
  "group_id": null,
  "budget_max": 50.0,
  "budget_spent": 0.0,
  "rpm_limit": 60,
  "tpm_limit": 100000,
  "expires_at": null,
  "disabled": false,
  "created_at": 1752537600,
  "deleted_at": null
}
```

Store `key` now - it is never shown again. `PATCH /admin/keys/{id}` takes
the same budget/quota fields (plus `disabled`) to adjust an existing key;
fields left out of the patch are unchanged, and an unknown id returns 400
`LM-1001`.

### Rotate a key (`POST /admin/keys/{id}/rotate`)

A lost or leaked key does not have to be replaced by a new one: rotation
mints a fresh secret for the **same** key record.

```bash
curl -s -X POST http://localhost:8080/admin/keys/<id>/rotate \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

The response has the exact same shape as creation - the new plaintext in
`key`, shown exactly once, plus the record. Everything else is preserved:
the `id` (so `usage_log` attribution is unbroken), the name, budgets,
accrued spend and quotas. The swap is applied to the in-memory table too,
so the old plaintext stops authenticating on the very next request and the
new one works without a restart. An unknown or deleted id returns 400
`LM-1001`.

### Delete a key (`DELETE /admin/keys/{id}`)

```bash
curl -s -X DELETE http://localhost:8080/admin/keys/<id> \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

Returns `204 No Content`. Deletion is a **soft delete** by design:
`usage_log` rows reference the key id, so removing the row would orphan the
key's usage history. Instead the row is tombstoned (`deleted_at` is set) and
kept for attribution and audit. A deleted key:

- stops authenticating **immediately** (the in-memory table is updated, no
  restart needed), and never loads again at boot;
- disappears from `GET /admin/keys`; pass `?include_deleted=true` to list
  tombstones (the audit view);
- rejects any further `PATCH`, `DELETE` or rotate with 400 `LM-1001`, like
  an unknown id - it cannot be resurrected by accident.

Retention of the tombstoned rows follows your usage-log retention policy:
they are plain rows in the same database, with no plaintext and no hash
that still authenticates. If you only want to pause a key, use
`PATCH {"disabled": true}` instead - that one is reversible.

### Store a provider key (`PUT /admin/provider-keys/{name}`)

```bash
curl -s -X PUT http://localhost:8080/admin/provider-keys/openai \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"key": "sk-..."}'
```

`{name}` is the provider's configured `name` (as in `[[providers]]`). The
body is `{"key": "<provider api key>"}`; a successful call returns `204 No
Content`. The key is sealed with AES-256-GCM under `LUMEN_MASTER_KEY` before
it touches disk, for providers whose `api_key_env` is unset or empty. The
call pings the hot-reload trigger after sealing the key, so the reloader
re-reads provider keys from the encrypted store (off the request path) and
rebuilds the provider registry - a rotated key takes effect without a
restart. Environment-sourced keys keep precedence over a stored key. See
[Deployment - Hot reload](deployment.md#hot-reload).

### Usage & spend reporting (`GET /admin/usage`)

Aggregates the [usage log](usage-log.md) per key, budget group, model,
provider or capability - the HTTP query surface over the same rows the
batched writer persists:

```bash
curl -s "http://localhost:8080/admin/usage?group_by=provider&since=2026-07-15T00:00:00Z" \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

Query parameters (all optional):

| Parameter | Meaning | Default |
|---|---|---|
| `key_id` | Only rows for this virtual key id. | all keys |
| `group_id` | Only rows attributed to this [budget group](#budget-groups) id. | all rows |
| `model` | Only rows for this client-facing model id. | all models |
| `provider` | Only rows attributed to this provider instance. | all providers |
| `capability` | `chat`, `embed` or `rerank`. | all capabilities |
| `since` | Window start (inclusive): unix seconds or RFC3339. | `until` - 24 h |
| `until` | Window end (inclusive): unix seconds or RFC3339. | now |
| `group_by` | `model`, `model_used`, `provider`, `capability`, `key_id`, `group_id`, `status` or `total`. | `model` |
| `limit` | Maximum groups returned, 1 to 1000. | 100 |

The response echoes the effective window and grouping, then one aggregate
per group - request counts split by status class, token totals, the
estimated-vs-upstream split ([ADR 003](../adr/003-token-accounting.md)),
rerank search units, media counts and cost:

```json
{
  "since": 1784073600,
  "until": 1784160000,
  "group_by": "provider",
  "truncated": false,
  "groups": [
    {
      "group": "openai",
      "requests": 1204,
      "requests_ok": 1180,
      "requests_client_error": 20,
      "requests_server_error": 4,
      "tokens_in": 803211,
      "tokens_out": 121408,
      "tokens_total": 924619,
      "estimated_requests": 17,
      "upstream_requests": 1187,
      "search_units": 0,
      "media_count": 3,
      "media_bytes": 402133,
      "cost": 12.41
    }
  ]
}
```

Groups are ordered by cost (highest first) and capped at `limit`; when more
groups matched, `truncated` is `true` and the returned groups are the most
expensive ones. A window that matches nothing is a normal `200` with an
empty `groups` array. Invalid filters, timestamps, `group_by` values or
limits are `400` `LM-1001`.

Two accounting notes:

- **Recent requests may lag.** Usage rows travel through the bounded
  channel and its batched writer (see
  [Usage log](usage-log.md#never-on-the-request-path)), so requests from
  the last flush interval (`usage_flush_ms`, default 2 s) may not appear
  yet - and entries dropped under a jammed channel
  (`lumen_usage_log_dropped_total`) never will.
- **`upstream_requests` counts rows whose numbers are exact**, which
  includes admission refusals (402/429): they consumed zero tokens, and
  zero is exact. `estimated_requests` counts rows whose token counts were
  locally estimated per ADR 003.
- **Provider attribution on refusals.** Rows served by a provider carry the
  provider that actually served them (under a fallback this may differ from
  the primary). Admission-refusal rows (402/429) never reached a provider;
  they carry the requested model's primary provider, so per-provider
  reports still see the traffic that was headed there.

One encoding note: an RFC3339 `+HH:MM` offset contains a `+`, which in a
query string means a space - percent-encode it as `%2B`
(`until=2026-07-15T03:00:00%2B02:00`), or use `Z`/unix seconds.

## Outbound webhooks for budget events

Everything above is *pull*: a control plane asks LUMEN what happened.
Webhooks are the *push* half (ADR 011), and they exist for one problem in
particular. A hard budget refuses with `402` `LM-4001` the instant the
pool empties, so a prepaid-credits backend that only polls will always
learn about the exhaustion *after* the customer has been refused. A
`budget.threshold` event at 80% is the trigger for an auto-recharge that
lands as a `POST /admin/keys/{id}/grant` before that ever happens.

**Absent by default.** With no webhook configured, LUMEN makes no outbound
call to anything but its providers, and does not even export a
`lumen_webhook_*` metric. Enabling it is a deliberate choice, and it
requires `auth.enabled = true` (every event describes a virtual key or a
budget group).

There are two ways to configure one, and they compose: the admin API for a
control plane, and the config file for a GitOps deployment.

### Creating a webhook through the API

This is the path a billing backend wants: nothing to restart, nothing to
edit on the host. Three calls, all gated by `LUMEN_MASTER_KEY`.

**1. Store a signing secret.** Generate one, keep your copy - LUMEN seals
it and will never hand it back.

```bash
export WEBHOOK_SECRET="whsec_$(openssl rand -hex 24)"

# The secret reaches curl on stdin, never in an argument list (any local
# process can read those), and `jq` builds the JSON so a secret containing a
# quote or a backslash cannot corrupt the body. `printf` is a shell builtin,
# so it does not put the value in the process table either.
printf '%s' "$WEBHOOK_SECRET" | jq -Rs '{secret: .}' |
  curl -s -X PUT http://localhost:8080/admin/webhooks/signing-key \
    -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
    -H 'content-type: application/json' \
    --data-binary @-
```

`204`. The secret is encrypted with AES-256-GCM under the master key
before it touches the disk, exactly like a stored provider key.

**2. Create the webhook.**

```bash
curl -s -X PUT http://localhost:8080/admin/webhooks \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "url": "https://backend.example.com/lumen/events",
    "events": ["budget.threshold", "budget.exhausted", "key.disabled"],
    "thresholds": [50, 80, 95],
    "channel_capacity": 1024,
    "timeout_ms": 5000,
    "max_attempts": 5,
    "retry_base_ms": 500
  }'
```

`url` is the only required field; every other one has the default shown
above. The `200` response is the new state:

```json
{
  "enabled": true,
  "source": "database",
  "settings": { "url": "https://backend.example.com/lumen/events", "...": "..." },
  "signed": true,
  "signing_key_stored": true,
  "updated_at": 1787691194
}
```

The new policy is in force from that moment: the very next request that
crosses a threshold enqueues an event. Delivery itself stays asynchronous
and best-effort, so a full queue or a receiver that stays down past the
retry budget still drops it (see the guarantees below). The settings are
stored, so a restart comes up identically.

**3. Check it.**

```bash
curl -s http://localhost:8080/admin/webhooks \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

`signed: true` is the field to watch. If it is `false`, deliveries carry no
`x-lumen-signature` and your receiver cannot tell them from anyone else's
traffic.

**Editing** is the same `PUT`: it replaces *every* setting, so send the
whole document rather than a fragment (there is no `PATCH` - a partial
update of a delivery policy is how you end up retrying against a URL you
meant to change). Every field is editable at runtime, `channel_capacity`
included: LUMEN rebuilds the queue and lets the previous sender drain what
it already had.

**Rotating the secret** is another `PUT /admin/webhooks/signing-key`. It
applies to the next delivery attempt, with nothing restarted. Retries of an
event already in flight keep the signature they were created with, so a
rotation never makes a pending retry unverifiable.

**Deleting**:

```bash
curl -s -X DELETE http://localhost:8080/admin/webhooks \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

Emission stops immediately, and the decision is stored: a `[webhooks]`
block in the config file will *not* quietly re-enable it on the next
reload. `DELETE /admin/webhooks/signing-key` forgets the secret separately.

### Declaring a webhook in the config file

For a deployment where the config file is the source of truth and no
control plane calls the API:

```toml
[webhooks]
url = "https://backend.example.com/lumen/events"
signing_key_env = "LUMEN_WEBHOOK_SECRET"
events = ["budget.threshold", "budget.exhausted", "key.disabled"]
thresholds = [50, 80, 95]
channel_capacity = 1024
timeout_ms = 5000
max_attempts = 5
retry_base_ms = 500
```

`signing_key_env` names the environment variable holding the secret; the
secret itself never appears in the file. A named-but-unset variable with no
stored secret is a **boot error**: a billing integration silently
downgraded to unsigned deliveries is worse than a refused start.

**Which one wins.** Settings written through `PUT /admin/webhooks` are
stored in the database and take precedence over this block, so a runtime
change is not undone by the next reload. `GET /admin/webhooks` reports
`source` as `"database"` or `"config"` so the two are never ambiguous. If
you manage the file, avoid the `PUT` (or expect the file to become
decorative); if you manage the API, the block is just the boot-time
default.

### The events

| Event | Fires when | What a backend does with it |
|---|---|---|
| `budget.threshold` | `budget_spent / budget_max` crosses a configured percentage | Charge the customer, then `grant` |
| `budget.exhausted` | The first `LM-4001` refusal since the subject last had headroom | Alert, upsell, or suspend cleanly |
| `key.disabled` | A `PATCH` disabled a key that was enabled | Mark the key inactive in your registry |
| `key.rotated` | `POST /admin/keys/{id}/rotate` succeeded | Invalidate any cached key material |
| `key.deleted` | `DELETE /admin/keys/{id}` tombstoned the key | Drop the key from your registry |

Both budget events fire for **keys and for budget groups**; the payload's
`scope` says which, and `subject_id` is the id you would pass to the
matching `grant` route.

### Edge-triggering

A threshold fires **once per budget epoch**, not once per request past it.
Cross 80% on Tuesday and every request for the rest of the week is silent.
A grant that buys headroom re-arms the thresholds it drops below: top a
$100 key that has spent $85 up to $300, and 85/300 = 28% re-arms 50% and
80% for the new epoch. A cap *reduction* does not un-fire what already
fired.

`budget.exhausted` works the same way: the first refusal signals, the next
thousand do not, and a grant re-arms it.

One consequence worth planning for: a restart re-arms from the **last
flushed** spend, so an event can legitimately fire twice if the crash
window (`auth.flush_interval_ms`, 10 s by default) swallowed the settle
that first crossed it.

### The payload

```json
{
  "id": "evt_9f2c1b7ad04e4a1c8f3b6e2d5a90c714",
  "event": "budget.threshold",
  "scope": "key",
  "subject_id": "3b14b24efc6dc198000cdf5506ddea6d",
  "subject_name": "team-search",
  "budget_max": 100.0,
  "budget_spent": 82.0,
  "threshold": 80,
  "ts": 1787691194
}
```

Accounting facts only. Never a plaintext key, never client metadata, never
prompt or response content - the same no-content construction as
`usage_log`. `budget_max` is omitted for an uncapped subject, and
`threshold` only appears on `budget.threshold`.

### Verifying a delivery

Every POST carries four headers:

| Header | Meaning |
|---|---|
| `x-lumen-signature` | Hex HMAC-SHA256 of the **exact** request body |
| `x-lumen-event-id` | Unique per *event*, not per attempt |
| `x-lumen-event` | The event kind, so you can route without parsing |
| `x-lumen-timestamp` | The event's own `ts`, which is inside the signed body |

Verify against the raw bytes you received, before any JSON round-trip:

```python
import hashlib, hmac

def verify(raw_body: bytes, header: str, secret: str) -> bool:
    expected = hmac.new(secret.encode(), raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```

The secret comes from whichever source is configured: the environment
variable named by `signing_key_env` wins when it is set and non-empty, and
the secret stored through `PUT /admin/webhooks/signing-key` fills in
otherwise. No route ever returns it, in any form; `GET /admin/webhooks`
reports the booleans `signed` and `signing_key_stored` and the variable
*name*, exactly as the provider surface reports key presence without key
material.

### Delivery guarantees, and what they are not

**At-least-once while the process lives.** A retryable failure (5xx, 429,
408, or a network error) backs off exponentially with jitter up to
`max_attempts`, reusing the same `x-lumen-event-id` every time. Any other
non-2xx is treated as permanent and not retried - the receiver has said
this event will never be accepted. Nothing is persisted, so a restart
forgets undelivered events.

**Receivers must be idempotent on `x-lumen-event-id`.** Retries, and the
post-restart re-fire described above, both re-send the same id.

**Not an accounting system.** A full queue drops events rather than slow a
request down, and a receiver that stays down past the retry budget loses
them. `GET /admin/usage/export` remains the source of truth: reconcile
against it on a schedule and treat webhooks purely as a latency
optimisation over polling.

**Never on the request path.** Detection is a compare on the atomic budget
settle that already happens per request; delivery is a non-blocking
`try_send` into a bounded queue drained by a background task. A dead
receiver cannot add a millisecond to a customer's request.

### Watching it work

| Metric | Meaning |
|---|---|
| `lumen_webhook_queued_total` | Events accepted into the queue |
| `lumen_webhook_sent_total` | Events the receiver acknowledged with a 2xx |
| `lumen_webhook_dropped_total` | Events dropped by a full queue - raise `channel_capacity`, or fix the receiver |
| `lumen_webhook_retries_total` | Failed attempts that were retried |
| `lumen_webhook_dead_total` | Events abandoned (retries exhausted, or a permanent rejection) |
| `lumen_webhook_delivery_seconds` | Wall time of a single delivery attempt |

These series appear the first time a webhook is enabled, not at boot, so a
gateway that never uses them stays quiet on `/metrics` too.

Sustained `dropped` or `dead` means your billing loop is running blind:
fall back to the export route until the receiver is healthy again.

### What a reload can change

A config reload re-resolves the same precedence: a stored row still wins,
so a reload never reverts a `PUT`. When the file block *is* what is in
force, every field is re-applied, and removing the block stops emission.

How a field is applied depends on the field. `url`, `events`, `thresholds`,
`timeout_ms`, `max_attempts` and `retry_base_ms` are retuned in place, on
the queue and sender task already running - so a retarget cannot lose what
is already queued. `channel_capacity` cannot be resized in place, so it
replaces both: new events go to the new queue while the previous sender
finishes delivering the events it had already accepted, then exits. Either
way nothing already accepted is discarded.

The one thing a reload cannot do is see a *new* environment variable: a
running process cannot observe a change to its own environment. Pointing
`signing_key_env` at a variable that was not set when the gateway started
needs a restart, or the sealed-secret route instead.

## Operator notes

Per [`SECURITY.md`](https://github.com/qdequele/lumen/blob/main/SECURITY.md),
protect `LUMEN_MASTER_KEY` and the SQLite database file **together**: either
one alone is not enough to read a stored provider key, but both together
decrypt it. Treat them as a single secret.
