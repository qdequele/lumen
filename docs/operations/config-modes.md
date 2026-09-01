# Config source modes: file and DB (ADR 012)

The dynamic half of LUMEN's configuration - providers, routing, resilience,
pricing, tokenizer, telemetry, image fetch, the hot-reloadable `[auth]`
knobs, and `[webhooks]` - lives in exactly one place at a time: either the
boot TOML file (today's default) or a table in the auth SQLite database. The
`config_source` boot key picks which. Both modes expose the identical admin
API, so a client cannot tell which one it is talking to.

## The two modes

```toml
config_source = "file"   # default
# config_source = "db"
```

**`"file"` (default).** The whole config - boot layer and dynamic layer
together - is the one TOML file the process was started with. `PUT
/admin/config` and every granular write stage a new copy, back up the
previous one to `<file>.bak`, and rename atomically; an external edit (a
human, GitOps) is picked up by the file watcher on the next reload. This is
unchanged from before ADR 012 - every existing deployment needs no change.

**`"db"`.** The dynamic layer lives in the `config_versions` table of the
same SQLite database `[auth]` already uses. Requires `auth.enabled = true`;
LUMEN refuses to boot otherwise (the admin plane is the only writer, and the
database is the store). The boot TOML file may then contain **only**
boot-layer keys - see below - since the DB is the dynamic layer's sole
source of truth and a second copy in the file would be exactly the kind of
drift ADR 012 exists to prevent.

## Boot layer vs. dynamic layer

A handful of settings are read once, at process start, and can never change
without a restart - the **boot layer**. Everything else is the **dynamic
layer**, owned by whichever `ConfigSource` is selected and editable through
the admin API without a restart.

| Boot layer (restart-only) | Dynamic layer (hot-reloadable, admin-API editable) |
|---|---|
| `server.host`, `server.port`, `server.body_limit`, `server.first_token_timeout_ms`, `server.sse_heartbeat_ms` | `[[providers]]` (routing, models, pricing, fallbacks) |
| `log_format` | `[resilience]` |
| `auth.enabled`, `auth.db_path` | `[telemetry]` |
| `config_source` itself | `[tokenizer]` |
| | `[image_fetch]` |
| | `[webhooks]` |
| | `auth.flush_interval_ms`, `auth.usage_channel_capacity`, `auth.usage_batch_max`, `auth.usage_flush_ms`, `auth.retention_days` |

In **file mode** this split is invisible day to day: the one file holds both
layers, exactly as before ADR 012. In **db mode** the boot file may hold
*only* the left-hand column's keys, under `[server]`, `log_format`, and
`[auth] enabled`/`db_path` (plus the top-level `config_source` key itself).
Any dynamic-layer key in that file - a `[[providers]]` block, a
`[resilience]` table, an `[auth]` key outside `enabled`/`db_path` - is a
**boot error** naming the offending key, never a silently-ignored second
source. The error message tells you the fix: remove the key from the boot
file, or set `config_source = "file"`.

The same split is enforced in the other direction on every admin write, and
just as strictly: in db mode, `PUT /admin/config` and every granular write
refuse a candidate that carries **any** boot-layer key at all -
`server.*`, `log_format`, `config_source`, or `auth.enabled`/`auth.db_path` -
independent of what value it names, even one identical to the field's own
built-in default. A dynamic document may never carry a boot-layer key; the
only way to change one is to edit the boot file and restart. See
[If-Match and the boot-layer guard](#if-match-and-the-boot-layer-guard)
below for why an equal-to-default value cannot be waved through.

A db-mode boot re-asserts `auth.enabled` and `auth.db_path` against the
stored document after the merge: `PUT /admin/config` (and every granular
write) is itself refused from ever touching `[auth] enabled`/`db_path` (see
[If-Match and the boot-layer guard](#if-match-and-the-boot-layer-guard)
below), so this is a belt-and-braces check against a document edited by
hand outside the API - it can never boot with auth silently disabled or
repointed to a different database file.

## First boot in DB mode

A fresh `config_source = "db"` deployment with nothing ever written to
`config_versions` boots into an empty-but-valid dynamic config: no
providers, every dynamic section at its built-in default. `GET /health` is
`200`, `/v1/*` answers the normal `LM-2001` (no such model) rather than
refusing to start, and the log carries:

```
config_source = "db" and no config stored yet; PUT /admin/config to install one
```

This is what makes a fleet bootstrap a pure API flow: boot the process with
just `[server]`/`[auth]`/`config_source = "db"`, then `PUT /admin/config`
with the real dynamic document once the process is reachable.

## The admin API surface

Every route below is mounted only when `[auth].enabled = true` and gated by
the master key, exactly like the rest of `/admin/*`. All of them work
identically in both modes, and every mutating one requires `If-Match`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/admin/config` | The current dynamic document, verbatim, plus its content hash. File mode: the whole config file, byte for byte. DB mode: the stored document, or an empty string with the empty document's hash before the first `PUT`. |
| `PUT` | `/admin/config` | Replace the whole dynamic document. |
| `GET` | `/admin/config/providers` | List every provider in the current document by `name` and `kind`, plus the document hash. |
| `GET` | `/admin/config/providers/{name}` | One provider's full config (its own fields, flattened) plus the document hash. |
| `PUT` | `/admin/config/providers/{name}` | Create or replace that provider (including its `models`). The path `{name}` must equal the body's own `name` field. |
| `DELETE` | `/admin/config/providers/{name}` | Remove the provider. |
| `GET` | `/admin/config/{section}` | One of `resilience`, `telemetry`, `tokenizer`, `image_fetch`, `webhooks`, `auth`. Response: `{"<section>": <value>, "hash": "<current hash>"}`. |
| `PUT` | `/admin/config/{section}` | Replace that section. `auth` is special - see [The `auth` section](#the-auth-section-five-knobs-only) below. |

Two admin surfaces are deliberately **untouched** by any of this:
`PUT /admin/provider-keys/{name}` (encrypted secrets, see
[Keys, quotas & budgets](keys-budgets.md#store-a-provider-key-put-adminprovider-keysname))
and `/admin/webhooks*` (ADR 011's DB-row-wins precedence over a `[webhooks]`
config block is unchanged, and applies to the document regardless of which
`ConfigSource` backs it). `GET /admin/config/webhooks` reads the `[webhooks]`
block from the *document* alone (`null` when absent) - a different thing
from `GET /admin/webhooks`, which reports whichever source is actually live.

### If-Match and the boot-layer guard

Every mutating call - the whole-document `PUT` and every granular one -
requires an `If-Match` header carrying the hash from a prior `GET`:

```bash
curl -s http://localhost:8080/admin/config \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY"
# {"config": "...", "hash": "b1946ac9..."}

curl -s -X PUT http://localhost:8080/admin/config \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'If-Match: "b1946ac9..."' \
  --data-binary @new-config.toml
```

- **Missing `If-Match`** is `400` `LM-1001`.
- **Stale `If-Match`** (the document changed since you read it - another
  operator's apply landed first, or in file mode a human edited the file
  directly) is `412` `LM-1004`. Re-`GET` and re-apply.
- **A candidate that changes a boot-layer key** (`server.*`, `log_format`,
  `auth.enabled`, `auth.db_path`, `config_source` itself) is `400`
  `LM-1001`, naming the changed key(s): `"restart-only keys changed:
  server.port; edit the boot config file and restart"`. In file mode an
  *unchanged* boot-layer block still passes, since the candidate there is
  the whole file.
- **A db-mode candidate that carries any boot-layer key at all** is `400`
  `LM-1001`, unconditionally - naming the key: `"unexpected key
  'auth.enabled' in dynamic document: boot-layer keys are restart-only in
  config_source = \"db\" mode; set them in the boot config file instead"`.
  This is stricter than the diff above, and deliberately so: the stored
  document in db mode never carries boot keys, so every boot field on that
  side already resolves to its built-in default, and a candidate that sets
  one EXPLICITLY to that same default (`[auth] enabled = false`, `[auth]
  db_path = "lumen.db"`, `[server] port = 8080`, ...) would otherwise
  produce no diff against the current document and pass straight through.
  But a restart merges the stored dynamic document OVER the boot file
  (`Config::load_with_dynamic`), so that stored key would silently win at
  the next boot regardless of whether its value ever differed from the
  default - refusing to boot at all if it disables auth, pointing at the
  wrong database, or silently rebinding the listen port. A well-formed
  db-mode candidate simply carries no boot-layer keys at all; this check
  does not compare values, it refuses the key outright.
- **A candidate that fails validation** (bad TOML, an unknown field, a
  dangling `fallbacks` reference, ...) is `400` `LM-1001`, naming the field
  or the dependent model.

Every write runs under one process-wide lock, so two concurrent applies can
never interleave and defeat the `If-Match` check. The candidate is fully
re-validated (parse, semantic checks, a throwaway provider-registry build)
before anything is persisted - the same validation a restart would run - so
a granular edit can never leave behind a document a restart would refuse to
load. On success the hot-reload trigger fires (when one is armed), so the
change is live without a restart.

### Granular edits preserve the rest of the document

`PUT /admin/config/providers/{name}` and `PUT /admin/config/{section}`
patch the current document with `toml_edit` rather than re-serializing it
from a parsed struct, so in file mode every comment and formatting choice
outside the table being touched survives untouched. In db mode the
document is machine-owned and this is harmless either way.

Deleting a provider that another model's `fallbacks` still references is
refused - `400` `LM-1001`, naming the dependent model - by the same
validation pass every other write goes through. Fetching an unknown
provider name or an unknown `{section}` is `404` `LM-1003`, the same style
every other per-entity admin lookup in LUMEN uses.

### The `auth` section: five knobs only

`GET`/`PUT /admin/config/auth` covers exactly the five dynamic `[auth]`
knobs - `flush_interval_ms`, `usage_channel_capacity`, `usage_batch_max`,
`usage_flush_ms`, `retention_days` - never `enabled` or `db_path`, which are
boot-layer. A `PUT` body naming either of those two is rejected `400`
(`deny_unknown_fields` on the request type), not silently ignored. Unlike
every other section, which the write replaces wholesale, `auth` merges
field-by-field into the existing `[auth]` table, so `enabled`/`db_path`
survive untouched even though the request body never mentions them:

```bash
curl -s -X PUT http://localhost:8080/admin/config/auth \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'If-Match: "b1946ac9..."' \
  -H 'content-type: application/json' \
  -d '{"flush_interval_ms": 5000, "usage_channel_capacity": 10000, "usage_batch_max": 500, "usage_flush_ms": 2000, "retention_days": 30}'
```

## DB-mode storage and retention

In db mode, every successful write inserts one immutable row into
`config_versions` (never updates one in place); the current document is
whichever row has the highest id. The newest 50 versions are retained -
older rows are pruned automatically on each write. There is no admin
endpoint to list or roll back to a previous version in this release (see
[Backlog](../backlog.md)); the rows exist and are queryable directly
against the SQLite file if you need to recover one by hand.

Secrets never enter the document in either mode: `api_key_env` and
`signing_key_env` are environment-variable *names*, never values, so a
`config_versions` row is exactly as sensitive as the config file is
today - no more, no less.

## Migrating between modes

Switching modes is an operational procedure, not an API call: it always
requires a restart, since `config_source` is itself boot-layer.

### File to DB

1. `GET /admin/config` against the running file-mode process and save the
   response body. In file mode this is the WHOLE config file, byte for byte
   (boot layer and dynamic layer together, see the admin API table above) -
   it still needs step 4 below before it is a valid db-mode candidate.
2. Edit the boot TOML: set `config_source = "db"`, and strip every
   dynamic-layer key (`[[providers]]`, `[resilience]`, `[telemetry]`,
   `[tokenizer]`, `[image_fetch]`, `[webhooks]`, and every `[auth]` key
   except `enabled`/`db_path`) - the boot file must contain only boot-layer
   keys once db mode is selected. Confirm `auth.enabled = true`.
3. Restart. The process boots with an empty stored document (first boot in
   db mode, see above) - `/health` is healthy, `/v1/*` answers `LM-2001`
   until the next step.
4. Before the `PUT`, strip every boot-layer key from the document saved in
   step 1: `[server]`, `log_format`, `config_source`, and `auth.enabled`/
   `auth.db_path` (keep the five dynamic `[auth]` knobs - `flush_interval_ms`,
   `usage_channel_capacity`, `usage_batch_max`, `usage_flush_ms`,
   `retention_days` - if the file set any of them). The saved document is
   the file-mode `GET`'s WHOLE file, and the db-mode boot-layer guard (see
   [If-Match and the boot-layer guard](#if-match-and-the-boot-layer-guard))
   now refuses a candidate carrying any boot-layer key unconditionally, even
   one whose value equals its built-in default - `PUT`-ing step 1's document
   as-is is refused `400` `LM-1001`.
5. `PUT /admin/config` with the stripped document from step 4, using the
   empty document's hash as `If-Match` (the `GET /admin/config` a fresh
   db-mode boot returns before any write).

### DB to file

1. `GET /admin/config` against the running db-mode process and save the
   response body - the dynamic document alone, no boot keys.
2. Merge it with the boot-layer keys the process is currently running with
   (`[server]`, `log_format`, `[auth] enabled`/`db_path`) into one TOML
   file.
3. Set `config_source = "file"` (or remove the key - `"file"` is the
   default) in that same file.
4. Point `--config` (or the existing config path) at the merged file and
   restart.

Either direction is a clean cutover: nothing reads from both sources at
once, and the old source (the boot file's dynamic keys in the file→db
direction, or the `config_versions` table in the db→file direction) is
simply no longer consulted after the restart - it is not deleted, so
reverting is the same procedure run in the other direction.

## See also

- [ADR 012 - Config source abstraction](../adr/012-config-source-abstraction.md)
  for the design rationale.
- [Keys, quotas & budgets](keys-budgets.md) for `[auth]` itself, and for the
  encrypted provider-key and webhook surfaces this document deliberately
  does not cover.
- [Deployment - Hot reload](deployment.md#hot-reload) for what a reload
  swaps and what stays restart-only outside the config-source boot key.
- [Error codes](../errors.md) for the full `LM-1001`/`LM-1003`/`LM-1004`
  taxonomy.
