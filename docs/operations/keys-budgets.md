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
  from memory to SQLite on `flush_interval_ms` (default 10000). A crash
  loses at most that much *accounting*; it never allows a budget overrun,
  since enforcement itself lives in memory.

## Refusals

| Code | HTTP | Cause |
|---|---|---|
| `LM-4001` | 402 | Hard budget exhausted. |
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
| `GET` | `/admin/keys` | List keys (records only - no hashes, no plaintext). |
| `PATCH` | `/admin/keys/{id}` | Adjust budget/limits, or enable/disable a key. |
| `PUT` | `/admin/provider-keys/{name}` | Store a provider API key encrypted at rest. |
| `GET` | `/admin/usage` | Aggregated usage and spend from the usage log. |

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
  "budget_max": 50.0,
  "budget_spent": 0.0,
  "rpm_limit": 60,
  "tpm_limit": 100000,
  "expires_at": null,
  "disabled": false,
  "created_at": 1752537600
}
```

Store `key` now - it is never shown again. `PATCH /admin/keys/{id}` takes
the same budget/quota fields (plus `disabled`) to adjust an existing key;
fields left out of the patch are unchanged, and an unknown id returns 400
`LM-1001`.

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

Aggregates the [usage log](usage-log.md) per key, model, provider or
capability - the HTTP query surface over the same rows the batched writer
persists:

```bash
curl -s "http://localhost:8080/admin/usage?group_by=provider&since=2026-07-15T00:00:00Z" \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
```

Query parameters (all optional):

| Parameter | Meaning | Default |
|---|---|---|
| `key_id` | Only rows for this virtual key id. | all keys |
| `model` | Only rows for this client-facing model id. | all models |
| `provider` | Only rows attributed to this provider instance. | all providers |
| `capability` | `chat`, `embed` or `rerank`. | all capabilities |
| `since` | Window start (inclusive): unix seconds or RFC3339. | `until` - 24 h |
| `until` | Window end (inclusive): unix seconds or RFC3339. | now |
| `group_by` | `model`, `model_used`, `provider`, `capability`, `key_id`, `status` or `total`. | `model` |
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

## Operator notes

Per [`SECURITY.md`](https://github.com/qdequele/lumen/blob/main/SECURITY.md),
protect `LUMEN_MASTER_KEY` and the SQLite database file **together**: either
one alone is not enough to read a stored provider key, but both together
decrypt it. Treat them as a single secret.
