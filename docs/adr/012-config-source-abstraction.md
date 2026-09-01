# ADR 012 - Config source abstraction: file and DB modes

- Status: accepted
- Date: 2026-08-29

## Context

Dynamic configuration (providers, routing, resilience, pricing, tokenizer,
telemetry, image fetch, hot-reloadable auth knobs, webhooks) lives in one
TOML file. `PUT /admin/config` already writes the file back atomically and
hot-reloads (ADR 010), so the file is the single source of truth for that
plane, while operational entities (virtual keys, budgets, usage, encrypted
provider keys, webhook settings) live in SQLite.

Three needs are unmet: granular config endpoints (edit one provider without
shipping the whole TOML), a file-less deployment mode for console-managed
fleets (ADR 010), and a guarantee that a human-edited file and API writes
can never silently diverge.

Prior art considered: Kong (mutually exclusive DB vs DB-less modes, node
settings always in a file), HAProxy Data Plane API (API mutations rewrite
the config file; the file stays authoritative), PostgreSQL `ALTER SYSTEM`
(machine-owned overlay with fixed precedence), LiteLLM `store_model_in_db`
(file and DB merged at load - the drift-prone anti-pattern we refuse).

## Decision

### 1. Boot layer vs dynamic layer, chosen at boot

The config schema splits logically (no file-format change): a restart-only
boot layer (server bind, log format, auth DB path, master-key env name, and
a new `config_source = "file" | "db"` key, default `"file"`) and the
hot-reloadable dynamic layer (everything else). In file mode one TOML holds
both layers, exactly as today. In DB mode the boot TOML may contain only
boot keys; a dynamic key present there is a boot error, never a silently
ignored second source. `config_source = "db"` requires auth with a
database. Every `Config` field is explicitly classified into one layer;
the classification is exhaustive so a new field must choose.

### 2. One canonical document behind a `ConfigSource` trait

The dynamic config is TOML text with a content-hash version, behind a trait
with `load()`, compare-and-swap `persist(toml, expected_hash)`, and change
notification. `FileSource` wraps the existing staged-write + `.bak` +
rename + watcher machinery. `DbSource` is an append-only `config_versions`
table (toml, hash, applied_at) in the existing SQLite; current = latest
row; persist is a single transaction that inserts only if the stored hash
matches; the newest 50 versions are retained. The version token exposed to
API clients is the content hash in both modes, so `If-Match` semantics
(`LM-1004` on staleness) are identical and clients cannot tell the modes
apart. Both backends are called only from `spawn_blocking` or the reload
task: the DB stays off the request path (pillar 3).

### 3. Granular endpoints share one mode-agnostic pipeline

New endpoints (`PUT|DELETE /admin/config/providers/{name}`, generic
`GET|PUT /admin/config/{section}` for scalar sections) and the existing
whole-document `PUT /admin/config` all run: apply-lock -> `load()` ->
`If-Match` check -> `toml_edit` patch (comments survive in file mode) ->
full boot-grade validation -> `persist()` -> hot reload. A granular edit
can never produce a document a restart would refuse. Boot-layer keys are
refused on every write path (`LM-1001`, "restart-only"). Secrets never
enter the document (env-var names only, as today), so `config_versions` is
no more sensitive than the file. `/admin/provider-keys` and
`/admin/webhooks` keep their exact semantics, including ADR 011's
DB-row-wins precedence.

### 4. Drift is impossible by construction, and a sick source never strips a gateway

Exactly one store is authoritative per deployment; there is no merge and no
seed-then-diverge copy. Concurrent writers are resolved by the hash CAS: in
file mode a human file edit changes the hash and a stale API write gets
412, in DB mode the transaction refuses the same way. On reload, a load or
validation error keeps the previous good config (the ADR 008 rule extended
to the whole document). First boot in DB mode with an empty table starts
with an empty-but-valid config (no providers) and logs that a `PUT
/admin/config` is expected, making fleet bootstrap a pure API flow.

## Consequences

- Operators choose per deployment: GitOps-able file (API still fully
  usable, HAProxy-style write-back) or file-less API-driven (Kong-style DB
  mode), with one shared code path and identical API contracts.
- Mode migration is an operational procedure, not a feature: `GET
  /admin/config` emits valid TOML in both modes; export/import CLI is
  backlog.
- Config history exists in DB mode (versions table) but has no API in v1;
  rollback endpoints are backlog.
- The `[auth]` section's field-by-field layer split must be maintained as
  fields are added; the exhaustive classification makes forgetting a
  compile-time error rather than a runtime surprise.
- No new error codes: `LM-1001`, `LM-1003`, `LM-1004` widen their
  documented meanings in `docs/errors.md`.
