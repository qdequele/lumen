# ADR 010 - Operator console and fleet control

- Status: accepted
- Date: 2026-08-19

## Context

LUMEN is administered by curl against `/admin`, guarded by one shared master
key. Running a gateway per region (for client round-trip latency) multiplies
that credential by the number of deployments and leaves no aggregate view of
spend, usage or provider health.

We want a web console that manages several gateway deployments: key and
budget administration, usage and cost dashboards, live operational state,
and provider configuration.

Constraints inherited from the pillars and prior ADRs:

- Admission and spend stay in per-process memory, before any upstream call;
  the database is never on the request path (M5, ADR 007).
- Config is a file, validated then swapped atomically; an invalid reload
  keeps the previous configuration (ADR 008).
- The gateway does not terminate TLS and delegates it to a reverse proxy
  (`docs/operations/deployment.md`).
- v1 is single-instance with auth enabled.
- Prompts are never persisted; `usage_log` has no request or response
  columns by construction.

## Decision

A console shipped as a **separate deployment**, not embedded in the gateway
binary, managing N independently configured gateways.

### 1. Tenancy: a project is pinned to one gateway

A project is bound to exactly one home gateway. Its virtual key lives on
that gateway and its budget is enforced there, in memory, unchanged.

This is the load-bearing decision. It means there is no cross-region key, no
budget spanning gateways, and therefore no shared auth state: no Postgres
backend for `virtual_keys`, no Redis for distributed rate limiting, and no
allocation of budget slices across regions. Every console mutation targets
exactly one gateway, so nothing in the design is a distributed transaction.

The cost is that moving a project between regions is a manual migration
(mint a key on the destination, retire the old one, accept that usage
history splits across two gateways). This is documented as an operational
procedure rather than built as a feature.

### 2. Control direction: push over a private network

The console calls each gateway's existing `/admin` API directly. No polling
agent is added to the gateway, and the admin API stays the one control
surface.

Push requires the console to reach the gateways, so the console runs on
operator-controlled infrastructure with private network reachability
(WireGuard or Tailscale). Platforms whose functions have no persistent
network identity are unsuitable, because they would force gateway admin
ports onto the public internet behind a credential that can mint keys.

### 3. Source of truth

The gateway remains authoritative for keys, budgets and live usage. The
console's database is a registry plus a cache, and stores no spend or budget
column. Disagreement is surfaced as drift, never auto-healed: the console
diffs `GET /admin/keys` against its own bindings and reports differences,
because auto-reconciliation would delete keys created deliberately through
the CLI.

There is one deliberate inversion. The gateway sweeps `usage_log` rows past
its retention window, so beyond that window the console is the only
remaining copy. Console retention must exceed gateway retention.

### 4. Human identity is delegated

The console implements no login, no password storage and no session
minting. It sits behind an OIDC reverse proxy and trusts a signed identity
assertion, exactly as the gateway delegates TLS to a proxy. Two roles:
`admin` mutates, `viewer` reads.

### 5. Two new admin endpoints

**`GET` / `PUT /admin/config`.** Config stays file-owned: the file remains
diffable, GitOps keeps working, and a gateway restarted without the console
comes up identically. `PUT` validates the submitted TOML and builds a
candidate registry from it *before* writing, because writing an invalid file
first would break the next SIGHUP or restart with no visible cause. The
write is atomic (temp file, fsync, rename) and guarded by `If-Match` against
a content hash returned by `GET`, so concurrent operators cannot silently
lose an edit. The previous file is kept for revert.

This endpoint can repoint a provider's `base_url` and thereby redirect
customer traffic. It is the highest-privilege operation in the system.
Consistent with decision 4, the gateway itself has no acting identity to
log - that lives in the console's own `audit_log`, keyed to the signed
identity the reverse proxy asserted. What the gateway records on its own
side is content-level, not identity-level: a successful apply logs the old
and new config content hashes at `info`, and a rejected apply (a stale
`If-Match`, invalid TOML, or a config the registry cannot build) logs the
full rejection detail at `warn`. Hashes only, never file content or secrets.

**`GET /admin/usage/export`.** Cursor-paginated raw `usage_log` rows.
`GET /admin/usage` aggregates over one dimension at a time, so building a
dashboard cube from it needs one call per dimension per window per gateway
and still cannot answer cross-dimensional questions. Raw export lets the
console build its cube once. Sovereignty is unaffected: `usage_log` holds no
prompt or response content, and the only caller-supplied field is the
ADR 002 `metadata` column.

## Consequences

- The July fleet-state blockers (shared Postgres for auth, Redis rate
  limiting, budget allocation across regions) are not required for this
  work. They return only if decision 1 is reversed.
- `lumen_usage_log_dropped_total` becomes user-visible. The bounded usage
  channel drops entries under pressure by design, which is correct for a
  gateway and wrong for a dashboard that would otherwise under-report
  silently; the console records the counter per collection window and warns
  for affected periods.
- A self-hoster now needs an OIDC provider before seeing a dashboard. This
  is mitigated with a documented forward-auth recipe and a loopback-only
  development bypass, not by weakening the model.
- `CLAUDE.md` lists "Web UI" under what v1 does not do. This ADR supersedes
  that line; billing remains out of scope.

## Alternatives considered

**Console embedded in the gateway binary.** Preserves the single-binary
pillar and needs no network trust model, but cannot manage more than the
gateway it ships inside, which defeats the multi-region goal.

**Pull agent.** Gateways poll the console for desired state. Removes the
inbound admin surface and is NAT-friendly, but adds a substantial new
subsystem to a gateway that deliberately does one thing, and reduces every
console mutation to "queued" rather than confirmed.

**Config owned by the database.** Would make the console the source of truth
for providers, but breaks GitOps, makes a console outage a configuration
outage, and diverges from the file-and-validate-then-swap model in ADR 008.

**Global keys with shared state.** Real cross-region budgets, at the cost of
a Postgres dependency on the auth path plus Redis for enforcement. Rejected
as unnecessary once a project is pinned to one gateway.
