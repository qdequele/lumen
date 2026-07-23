# ADR 009 - Shared parent budgets (budget groups)

- Status: accepted
- Date: 2026-07-22

## Context

A virtual key is today the only budget boundary: `budget_max` lives on the key
and is enforced in memory by a CAS reservation (M5, refined by ADR 007). That
model cannot express a common billing shape: one prepaid pool consumed by
several keys. The concrete motivator is a control plane that sells credits per
customer while issuing one key per project of that customer; today it must
chunk-allocate `budget_max` across the project keys and rebalance them from
outside, which is racy and leaves credits stranded on idle keys.

Constraints inherited from the pillars and prior ADRs:

- Admission stays in per-process memory, before any upstream call; the
  database is never on the request path (M5).
- A request refused at admission consumes nothing: quota bumps and budget
  reservations made by earlier admission steps are unwound (ADR 007).
- Admin mutations apply to the DB and the in-memory state together, with no
  restart; hot reload re-reads DB state and never strips working state on a
  read error (ADR 008).
- v1 is single-instance with auth enabled; nothing here changes that
  (`docs/operations/deployment.md`, Scaling and high availability).

## Decision

Introduce **budget groups**: a named budget pool that any number of virtual
keys can belong to. Admission checks the key's own budget (when set) AND the
group's budget (when the key belongs to one); spend settles against both.

### 1. Model

A group is `{id, name, budget_max, budget_spent, created_at, deleted_at}`.
`budget_max = NULL` means unlimited (a pure attribution container). Groups
carry **budget only** in this slice: no group RPM/TPM, no disabled flag, no
expiry (future work, below). A key references at most one group via a nullable
`group_id`; membership is admin-managed, never config-file state (groups and
keys are DB entities, unlike providers).

`budget_spent` on the group is its own accumulator, flushed like key spend; it
is never recomputed from member keys. Spend that member keys accrued before
joining (or after leaving) the group is not retroactively moved.

### 2. Admission and settlement

`admit` order becomes: RPM bump, TPM bump, key budget reserve, **group budget
reserve**. A refusal at any step unwinds every earlier step (ADR 007 rule),
so a request refused by the group consumes no key quota and no key budget.
The `Reservation` additionally holds the group entry and the amount reserved
against it, captured at admission:

- `settle(actual_cost, actual_tokens)` applies the cost delta to the key AND
  the group (the TPM adjustment is key-only; groups have no TPM).
- Dropping unsettled refunds both budget reservations; the TPM debit stays,
  per ADR 007.
- A key moved to another group mid-flight settles against the group captured
  at admission - spend is attributed to the pool that admitted it.

Both refusals reuse **LM-4001 (402)**: the semantic is unchanged, "a hard
budget you are subject to is exhausted". The error *message* distinguishes
the scope ("budget exceeded for this key" vs "budget exceeded for this key's
group") via a `scope` field on `GatewayError::BudgetExceeded`; there is no
probing concern because a caller can only observe budgets it is billed
against. The key-scope message is byte-identical to today's.

### 3. In-memory state

`AuthState` gains `groups: DashMap<String, Arc<GroupEntry>>`. `GroupEntry` is
the group analogue of `KeyEntry`'s budget half: `budget_max_micro`,
`spent_micro`, `dirty`, same micro-USD atomics, same CAS reservation loop.
`KeyEntry` holds `group: ArcSwapOption<GroupEntry>` (lock-free load on the
hot path; `arc-swap` joins the auth crate from the existing workspace
dependency). The pointer is resolved by `AuthState` (boot load, upsert,
apply), never by `KeyEntry::from_record`. A `group_id` that cannot be
resolved in memory (only reachable through out-of-band DB edits) leaves the
key without live group enforcement and logs a warning - fail-open for that
key rather than refusing all its traffic.

### 4. Flush, shutdown, reload, crash

- The periodic flusher drains dirty groups exactly like dirty keys
  (`drain_dirty_groups` alongside `drain_dirty`) into
  `persist_group_budgets`; the shutdown drain does the same final flush.
- Hot reload re-reads groups from the DB **before** re-reading keys (keys
  resolve group pointers), upsert-only, limits re-applied, in-memory spend
  preserved, DB errors keep the current tables (ADR 008 semantics).
- Crash recovery is identical to keys: enforcement lives in memory ahead of
  the flush, so a running process never overruns; a crash loses at most
  `flush_interval_ms` of *accounting* per pool, and after restart budgets
  reload from the last persisted state.

### 5. Storage and usage attribution

Migration `0007_budget_groups.sql`:

- `CREATE TABLE budget_groups (...)` as in §1;
- `ALTER TABLE virtual_keys ADD COLUMN group_id TEXT` (nullable; no SQLite FK,
  consistent with the schema's existing app-level referential integrity);
- `ALTER TABLE usage_log ADD COLUMN group_id TEXT` plus an index.

Every usage row (success AND admission refusal, per ADR 007) is stamped with
the key's group id captured at accounting begin, so per-pool reporting works
even for refused traffic. `GET /admin/usage` gains `group_by=group_id` and a
`group_id` filter, both riding the existing closed-set plumbing.

### 6. Admin surface

- `POST /admin/groups` `{name, budget_max?}` - create (201, the record; no
  secret exists for a group).
- `GET /admin/groups` (`?include_deleted=true` for tombstones) - list.
- `PATCH /admin/groups/{id}` `{name?, budget_max?}` - adjust; spend preserved.
- `DELETE /admin/groups/{id}` - **soft delete**, refused (400, LM-1001) while
  the group still has active member keys; the tombstone keeps `usage_log`
  attribution, mirroring key deletion, and its final spend is flushed on
  removal from memory.
- `POST /admin/keys` accepts `group_id`; `PATCH /admin/keys/{id}` accepts
  `group_id` as a tri-state field (absent = unchanged, `null` = leave the
  group, string = join) - the one deliberate divergence from the "patches
  cannot clear to NULL" rule, because leaving a group must not require
  re-minting the key. A `group_id` naming an unknown or deleted group is
  refused (400, LM-1001) before any write.
- `lumen keys create` gains `--group-id` (validated against the DB);
  a `lumen groups` offline subcommand is deferred - groups have no bootstrap
  chicken-and-egg problem because `/admin/groups` needs only the master key.

Every mutation applies to the DB and the in-memory maps together, effective
on the next request, no restart - the ADR 008 contract.

## Consequences

- The prepaid-credits control-plane pattern collapses to: one group per
  customer, one key per project, top up the group. No chunk allocation, no
  rebalancing, no stranded credits.
- The hot path gains one `ArcSwapOption` load and, for grouped keys, one more
  CAS loop per request - same order of cost as the existing key reservation,
  still allocation-free, still no locks held across await points.
- Two pools can now refuse a request; operators must read the LM-4001 message
  (or the usage log's 402 rows grouped by `group_id`) to see which.
- `VirtualKeyRecord` grows `group_id`, so every enumerated column list
  (`load_auth_entries`, `list_keys`, `find_by_hash`, `fetch_key`, the
  integrity dump) changes in lockstep; the admin key responses expose it.
- **Known admin-plane race, accepted for v1.** `delete_group`'s member
  count and tombstone are separate statements, as are the membership
  validation and write in key create/patch, so a key racing into a group
  being deleted can orphan onto the tombstone. The consequence is bounded
  and fail-open by design: live `Arc` holders keep enforcing against the
  detached pool, and after a reload the dangling `group_id` resolves to
  no-pool-enforcement with a warning. Admin mutations are expected to be
  serialized by the control plane; wrapping count+tombstone (and
  validate+write) in `BEGIN IMMEDIATE` transactions is the cheap hardening
  if that assumption ever breaks.
- Not in this slice (recorded in `docs/backlog.md`): group RPM/TPM, a group
  `disabled` flag, group expiry, nested groups, a
  `lumen_group_budget_remaining` gauge, and a `lumen groups` CLI.
