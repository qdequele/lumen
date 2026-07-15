# ADR 008 - Hot reload for auth knobs and DB provider-key rotation

- Status: accepted
- Date: 2026-07-15

## Context

Hot reload (ADR-adjacent, M7 §7.3) already re-validated the config on SIGHUP or a
config-file change and atomically swapped the routing table, price table and
resilience policy (circuit-breaker state preserved). Two surfaces were still
boot-time only and were flagged as debt (issue #20, `docs/backlog.md` M5/M7):

1. The `[auth]` operational knobs (budget-flush cadence, usage-log retention,
   usage-writer channel sizing) were read once and baked into background tasks.
2. A provider key stored in the encrypted DB via `PUT /admin/provider-keys`
   only took effect at the next restart: the DB-key snapshot was captured once at
   boot and merely re-applied unchanged on every reload, so a *rotation* was
   invisible until a restart.

The overriding constraints are the repo pillars: **DB stays off the request
path** (pillar 3), **no secret leaks** (STRICT rule 5), and reload must remain
atomic and non-disruptive to in-flight requests. Three questions had to be
resolved that the specs do not cover.

## Decision

### 1. Server bind address stays restart-only (explicitly out of scope)

The issue lists the bind address among "read once at boot", but rebinding a live
`TcpListener` under load is high-risk (dropped connections, port races, partial
failure with no clean rollback) for negligible benefit over a rolling restart.
We deliberately do **not** hot-rebind host/port. This is documented as a
permanent restart-only limitation in the `reload` module docs and the backlog,
rather than left as an implied gap.

### 2. DB provider keys are re-read on every reload, in the reload task

Instead of a frozen boot snapshot, `ReloadTargets` now carries an optional
`ProviderKeySource` (an owned `KeyStore` clone + its own `MasterKey` handle + the
configured provider names) and the backfill lives in an
`Arc<ArcSwap<HashMap<..>>>`. Each reload runs `reload_once`, which:

1. **async, in the reload task** (never the request path): re-reads and decrypts
   every configured provider key from the store into a fresh map and stores it
   into the `ArcSwap`. A DB/decryption error is logged and the previous snapshot
   is kept, so a sick DB can never strip a working key.
2. **on a blocking thread**: runs `apply_reload`, which reads the refreshed
   snapshot and merges it into env-keyless specs before the (fallible) registry
   rebuild, then swaps the registry `ArcSwap` atomically.

Environment variables keep precedence (a spec with a resolved env key is left
untouched), so rotation via this route only affects env-keyless providers, which
matches the existing "DB back-fills providers whose `api_key_env` is unset"
model. The registry is rebuilt wholesale (the same path SIGHUP already used); a
key rotation does not change routing, only the `api_key` carried by rebuilt
providers.

### 3. `PUT /admin/provider-keys` triggers a reload via a shared `Notify`

To make a rotation apply without waiting for a SIGHUP or file touch, the admin
handler pings a shared `tokio::sync::Notify` after the DB write completes; the
reloader task selects on it as a third wake source alongside SIGHUP and the file
watcher. The trigger is exposed on `AppState` only when the reloader is actually
armed, so the admin API never claims a rotation was applied when no reloader is
running. The DB write is awaited before the notify, so the reload always observes
the new key. Coalescing is free: `Notify` collapses a burst into one wake.

### 4. Auth knobs are a live atomic cell, not task-local constants

The runtime-safe knobs (`flush_interval_ms`, `retention_days`) live in a shared
`AuthKnobs` (two atomics). The budget-flush task became a `sleep`-loop that reads
the interval each cycle (a `tokio::time::Interval` cannot have its period changed
in place), and the retention-purge task reads the window each tick. A reload
overwrites the atomics, so both tasks pick up new values on their next tick with
no restart. `.max(1)` guards a reload that disables auth (knob -> 0).

The bounded usage-log channel knobs (`usage_channel_capacity`, `usage_batch_max`,
`usage_flush_ms`) are **not** made reloadable: the channel capacity is structural
(fixed when the `mpsc` is created and the `UsageLogger` clones are handed to the
app), so changing it means re-plumbing the writer - a restart is the honest
boundary. `auth.enabled` and `auth.db_path` are likewise structural (they decide
whether the whole stack and its DB connection exist).

## Consequences

- Rotating a provider key is now a live operation: `PUT /admin/provider-keys`
  then requests authenticate with the new key within one reload, no restart.
- The reload's DB read is bounded to the configured provider set and runs only in
  the reload task, honouring "DB off the request path".
- Keys are never logged: `ProviderKeySource` holds a redacted `MasterKey`
  (zeroized on drop) and the backfill map carries raw keys only in memory, merged
  into `ProviderSpec` whose `Debug` already redacts `api_key`.
- The restart-only surface (bind address, `enabled`, `db_path`, channel sizing)
  is now explicitly documented rather than an accident of implementation.
