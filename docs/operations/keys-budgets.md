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
it touches disk, and is read back only at boot, for providers whose
`api_key_env` is unset or empty - a stored key takes effect at the next
restart.

## Operator notes

Per [`SECURITY.md`](https://github.com/qdequele/lumen/blob/main/SECURITY.md),
protect `LUMEN_MASTER_KEY` and the SQLite database file **together**: either
one alone is not enough to read a stored provider key, but both together
decrypt it. Treat them as a single secret.
