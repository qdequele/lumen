# ADR 020 - Sub-groups: nested budget pools under a team's lease

- Status: Accepted
- Date: 2026-11-06
- Builds on: ADR 009, ADR 015, ADR 018

## Context

On the hosted gateway each Lab account has exactly one budget group, the
**lease** (ADR 015 decision 2). The Lab creates it (`lab-<account id>`) and
keeps `budget_max` at the spend it has accounted for plus the team's credit
balance converted to provider cost (saas `LumenAccountGroup.push_lease`).
Lumen refusing a request at that cap is how credits are enforced.

Teams therefore cannot create budget groups on the hosted gateway. The Lab
refuses every write under `/admin/groups` there (saas
`InstanceAllowlist::LUMEN_PLATFORM_ONLY_WRITES`), the console hides the
actions (`canWrite = ... && !instance.builtin`), and the proxy forces every
key into the lease (`key_body_in_group`). The refusal is right for the
current model: a second group of the account would have its own
`budget_max`, so creating one, raising its cap or granting to it would let
the team spend past its credits.

Teams still want what groups give a self-hosted operator: one shared pool
for a subset of keys ("staging: 2,000 credits", "customer-acme: 10,000
credits") and spend reported per pool. Per-key budgets do not cover it. They
strand budget on idle keys, the problem ADR 009 was written to solve.

The constraint: **nothing a team can write may raise what the account can
spend above the lease.**

## Decision

Make budget groups nest one level deep. A **sub-group** has a `parent_id`
naming a top-level group. A request from a key in a sub-group must fit the
key's budget, the sub-group's budget **and** the parent's budget. The parent
is always enforced, so a sub-group's cap can only restrict spending, never
widen it. On the hosted gateway the parent is the lease: teams get full
control of sub-groups while the lease stays the Lab's.

The feature is generic Lumen. A self-hosted operator can use it for
department → team pools too.

### 1. Model (Lumen)

`budget_groups` gains `parent_id TEXT NULL` (migration
`0018_group_parent.sql`, additive, plus an index on `parent_id`). Rules,
checked at the admin API before any write:

- **Depth 1.** The parent must be a live group with no parent itself.
- **Immutable.** `parent_id` is set at creation and never changes (`PATCH`
  with `parent_id` is `400 LM-1001`). To move pools, create a new sub-group
  and move the keys.
- **Inherited billing identity.** A sub-group takes its parent's
  `account_ref` and is billable exactly when the parent is (ADR 015 decision
  3). `account_ref` is refused on a sub-group create or patch, and refused
  on a patch of a parent that has live sub-groups, because billing must
  never split between a pool and its parent.
- **Inherited bandwidth rate.** `bandwidth_micro_usd_per_gb` is refused on a
  sub-group. Bandwidth is charged at the parent's rate (§3).
- **Delete.** A parent with live sub-groups cannot be deleted (`400
  LM-1001`), like a group with live member keys today. A sub-group is
  soft-deleted like any group.
- `budget_max = NULL` on a sub-group stays legal: a pure attribution pool,
  capped only by its parent.

A key still references exactly one group, either a top-level group or a
sub-group. `group_id` semantics on keys do not change.

### 2. Admission and settlement

`admit` becomes: RPM, TPM, key budget, group budget, **parent budget**. A
refusal at any step unwinds every earlier step (ADR 007). The reservation
already captures the key's group at admission. The parent is reached through
it (`GroupEntry::parent`), so a key moved mid-flight still settles against
the pools that admitted it.

- `settle` applies the cost delta to the key, the group and the parent.
- Dropping an unsettled reservation refunds all three.
- The parent's `spent_micro` therefore always includes every sub-group's
  spend. The Lab's lease sync reads the parent's `spent_micro` and needs no
  change.

A parent refusal is `LM-4001` with a new `BudgetScope::ParentGroup`
("budget exceeded for this key's parent group"). Lumen sends no Lab-specific
wording. The Lab console shows a parent refusal as "your team is out of
credits".

### 3. Team provider keys and bandwidth follow the parent

ADR 018 keys team-provider-key pools and the bandwidth rate by the
authenticated key's group. For a key in a sub-group both resolve through
the **top-level group** (`GroupEntry::billing_root()`):

- BYOK pools: `byok.rs` looks pools up by the root's id, so a team's own
  provider keys serve every sub-group and are never configured per
  sub-group. Scoped routes `/admin/groups/{sub}/byok/*` answer `404
  LM-1003`.
- Bandwidth is charged at the root's rate to all three accumulators (key,
  sub-group, root).

Virtual-model routing conditions on `group` (ADR 014) are the exception:
they see the key's **own** group, the sub-group, like the usage log. A rule
written for a top-level group does not match its sub-groups' keys; name the
sub-groups in the rule when it should.

### 4. In-memory state

`GroupEntry` gains `parent_id` and `parent: OnceLock<Arc<GroupEntry>>`. A
set-once cell, not an `ArcSwap`, works because `parent_id` is immutable: the
hot path reads it without a lock. `AuthState::load` and hot reload upsert
top-level groups before sub-groups, and keys after both.

A `parent_id` that does not resolve in memory (an out-of-band DB edit, or a
reload racing an admin write) **fails closed** for that sub-group: its keys
are refused with `500 LM-5001`, not `LM-4001`, because the team is not out
of budget and must not be told to buy credits (rule 8: no misleading client
error for an internal fault). A warning is logged. The link heals on the
next upsert that finds the parent (a hot reload), with no restart.

This is the opposite of ADR 009's fail-open for a dangling key `group_id`.
A missing parent means the lease is not enforced, and the hosted gateway
must never serve unbilled spend.

Hot-path cost: one more cell load and, for keys in sub-groups only, one
more CAS loop. Still allocation-free and lock-free. Keys in top-level groups
pay a single empty-cell check.

The admin writes that check sub-group rules (create under a parent, patch
of `account_ref`, delete of a parent) read and write in one `BEGIN
IMMEDIATE` transaction, so a sub-group cannot appear between the check and
the write.

### 5. Admin surface

- `POST /admin/groups` takes `parent_id?`. On a scoped call
  (`X-Lumen-Account-Ref`) the parent must be in scope (else `404 LM-1003`,
  like any foreign id) and the sub-group inherits the account.
- `GET /admin/groups` and `GET /admin/groups/{id}` expose `parent_id`. The
  list accepts `?parent_id=` to filter.
- `PATCH /admin/groups/{id}` and `POST /admin/groups/{id}/grant` work on
  sub-groups unchanged (grant still needs a cap).
- `GET /admin/usage` is unchanged. `usage_log.group_id` records the key's
  own group (the sub-group), so `group_by=group_id` reports per sub-group,
  and the parent's total is its `spent`.
- `docs/openapi.yaml`: `parent_id` on the group schemas and the new LM-4001
  scope.

### 6. Lab side (meilisearch-lab)

The proxy keeps the lease untouchable and lets everything else through.
None of the checks below can widen the lease, because §2 makes the lease
bind regardless. They exist so a team cannot create a second top-level group
or edit the lease itself.

**Proxy (`InstanceProxyController`, `InstanceAllowlist`), hosted Lumen
only:**

- Replace the blanket `LUMEN_PLATFORM_ONLY_WRITES` with targeted refusals:
  any write to `/admin/groups/<lease id>` or `/admin/groups/current` (team
  key routes excepted, as today) is `403 platform_only`.
- `POST /admin/groups`: force `parent_id` to the lease, as keys are forced
  into it today. Refuse `account_ref` and `bandwidth_micro_usd_per_gb` in
  the body. Check that the response's `parent_id` equals the lease. If it
  does not (a gateway older than this ADR ignored the field and created a
  top-level group), delete the group and answer `502 instance_outdated`.
  This guard makes a wrong release order safe.
- `PATCH`, `DELETE` and `grant` on `/admin/groups/{id}`: read the group
  (scoped) and require `parent_id == lease`, else `404`. This also covers any
  stray top-level group left from before.
- Keys (`key_body_in_group`): a missing or `null` `group_id` still means the
  lease. A named `group_id` must be the lease or one of its sub-groups (one
  scoped `GET`), else `400 invalid_group`. The Lab's managed keys
  (playground, Scrapix, Pipelines) stay in the lease and stay read-only.
- Roles do not change: members manage sub-groups, as they manage keys.

**Lease sync (`LumenAccountGroup`):** no change. `push_lease` reads the
lease's `spent_micro`, which now includes sub-group spend (§2). Usage events
of sub-group keys carry the inherited `account_ref`, so `accounted_micro`
already counts them.

**Console:**

- Budget groups page, hosted: pin the lease as the first row, "Team budget,
  funded from your credits", with no row actions and a "Buy credits" link.
  Sub-groups follow it with Edit, Grant and Delete. "Create group" is shown
  to members. Its dialog reads: *"Draws from your team's credits. Requests
  stop when this group's budget or your credits run out, whichever comes
  first."*
- Sub-group budgets are entered and shown in **credits**, with the same
  conversion as hosted key budgets (`lib/lumen/credits.ts`), and stored in
  Lumen as provider-cost USD.
- Key dialog, hosted: the group select lists "Team budget" (default) and the
  sub-groups, with no "No group" option.
- Usage page: the existing group filter and `group_by=group_id` pick up
  sub-groups with no change.
- Show `BudgetScope::ParentGroup` refusals as "out of credits" with a Buy
  credits action.

**MCP (`McpToolset`):** drop the "Meilisearch manages the team's budget
group" caveat on `gateway_create_group` and the other group tools. Describe
the hosted behaviour instead: groups are created under the team budget,
budgets are in credits, and the team budget itself is read-only.

**Docs:** `docs/content/docs/gateway/overview.mdx` (budget groups on the
hosted gateway). Re-vendor `contracts/vendor/lumen/openapi.yaml`.

**glutony:** no change. The Pipelines key stays in the lease. Putting it in
its own "Pipelines" sub-group for attribution is a possible follow-up.

## Rollout

1. **Lumen**: migration 0018, nesting, scoping, BYOK and bandwidth
   resolution, OpenAPI, CHANGELOG. Existing groups have no parent, so
   behaviour is unchanged until something sets one. Release, then deploy
   the hosted gateway.
2. **Lab**: re-vendor the contract, then ship the proxy, console, MCP and
   docs changes. Ship only after the hosted gateway runs step 1. The
   `parent_id` echo check in §6 covers a mistaken order.

No new env vars or secrets. The migration is additive and forward-only.

## Alternatives considered

- **The Lab slices the lease across sibling top-level groups.** Each team
  pool would get a share of the credits and the Lab would rebalance. This
  was rejected: it strands credits on idle pools and races on rebalancing,
  the exact shape ADR 009 replaced.
- **Per-key budgets only (status quo).** No shared pool across a subset of
  keys, and spend is reported per key, not per pool.
- **Arbitrary nesting depth.** No current use case. Depth 1 keeps admission
  at a fixed two group CAS loops and makes the immutable-parent and
  delete rules simple. It can be relaxed later without a schema change.
- **Mutable `parent_id`.** It would need an `ArcSwap` on the hot path and
  flush-first billing rules like ADR 015 decision 4. Moving keys covers the
  need.

## Open questions

1. **Defense in depth inside Lumen.** Today the lease's protection rests on
   the Lab proxy (§6). A `managed` flag on a top-level group, refusing
   scoped `budget_max`, `grant` and delete, would move that guarantee into
   Lumen. It requires the Lab's lease sync to call unscoped. Recommended
   as a follow-up, not a blocker: per §2 a sub-group write can never widen
   the lease anyway.
2. **Periodic budgets** ("2,000 credits per month" per sub-group) are out of
   scope. Lumen has no budget periods today.
3. **Who may create sub-groups**: members (as proposed, same as keys) or
   admins only?

## Tests

- Lumen `auth`: admission with a refusal at each of key / group / parent,
  unwinding everything; settle and drop on all three; a parent cap binding
  below a larger sub-group cap; fail-closed on a dangling parent; load
  order on boot and reload.
- Lumen `server`: create, patch and delete rules (depth, immutability,
  `account_ref` and bandwidth refusals, parent with sub-groups);
  scoped-create inheritance and foreign-parent 404; BYOK and bandwidth
  resolving to the root; the usage event of a sub-group key billed to the
  account.
- Lab saas: the lease and `current` are refused, create forces the parent
  and rolls back without the echo, writes to a non-child are refused, keys
  join only the lease or its sub-groups, managed keys are untouched.
- Lab console: groups page hosted (lease pinned and read-only, actions on
  sub-groups, credits units), key dialog group options.
