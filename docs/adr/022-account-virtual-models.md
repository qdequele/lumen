# ADR 022 - Account virtual models

- Status: accepted
- Date: 2026-11-08
- Amends: ADR 014
- Builds on: ADR 009, ADR 012, ADR 015, ADR 018, ADR 020

## Context

The hosted gateway (ADR 015) serves every Lab team through one Lumen. Each
team is a Lab account: the Lab calls the admin API with the master key plus
`X-Lumen-Account-Ref: <account uuid>` and Lumen scopes keys, budget groups
and usage to that account (platform contract v2 section 8.3).

Virtual models (ADR 014) live only in the dynamic config document (ADR 012),
which is platform-owned: the `/admin/config*` routes answer `403 LM-4005` to
a call carrying the account header. Teams therefore cannot have virtual
models of their own.

Putting team models into the shared document was rejected:

- one id namespace and one hash for every team, so two teams' saves conflict
  (`412 LM-1004`) although they touch unrelated models;
- every team would see, and could call, every other team's models;
- Lumen refuses to delete a foundation model a virtual model references
  (`LM-1001`), so one team's model would block staff from retiring a platform
  model;
- every team edit would hot-reload and revalidate the whole gateway.

The Lab already ships a console UI and proxy rules over
`/admin/config/virtual_models`, `/{id}` (GET, PUT and DELETE with `If-Match`)
and `/{id}/plan` for self-hosted gateways (meilisearch/lab#133). Keeping that
request and response shape lets the Lab reuse it for hosted teams.

## Decision

### 1. Storage: one document per account, outside the platform document

Account virtual models live in the auth database (migration
`0019_account_virtual_models.sql`), one row per account:
`account_ref` (canonical lowercase UUID, primary key), `document` (a JSON
array of the ADR 014 virtual-model objects, in the account's order), `hash`,
`generation` and `updated_at`.

- **`hash`** is the BLAKE3 hex digest of the document text. It is the
  account's `If-Match` version, exactly like the ADR 012 hash is the
  platform's. An account without a row has the hash of the empty document
  `[]`, so its first write uses the hash a `GET` returned.
- **Writes are a compare-and-swap** in one `BEGIN IMMEDIATE` transaction: the
  write applies only when the stored hash (or the empty hash, without a row)
  equals `If-Match`, else `412 LM-1004`. A stale account write never touches
  another account or the platform document, and a team write never changes
  the platform hash.
- **`generation`** is one counter across all rows: each write sets it to the
  table's maximum plus one. A row whose last model is deleted keeps its row
  (document `[]`), so the counter sees deletes too.

The platform document, its hash, its validation and its hot reload are not
touched by a team write.

### 2. Admin API: the same routes, scoped by the header

`GET /admin/config/virtual_models`, `GET|PUT|DELETE
/admin/config/virtual_models/{id}` and `GET .../{id}/plan` accept
`X-Lumen-Account-Ref`. With the header they read and write that account's
document; without it they behave exactly as before (the platform document).
The other `/admin/config*` routes stay platform-only.

The alternative, parallel routes (`/admin/accounts/{ref}/virtual_models`),
was rejected: the header already means "scope this call to that account" on
the keys, groups, usage and BYOK routes, the Lab proxy already adds it, and
the same paths with the same shapes let the Lab reuse its self-hosted UI and
proxy rules by allowing the header on these routes. A second path would be a
second contract to document, test and vendor for no gain.

The shapes are unchanged: the list is `{virtual_models, hash}`, a `GET` is the
model's fields plus `hash`, `PUT` and `DELETE` require `If-Match` and answer
`204`, ids are percent-encoded in the path (`acme%2Ffast`). One additive
field: each list entry may carry `warnings` (see 4). A scoped call works in
both config modes (file or DB): it never reads the platform document.

### 3. Ids and resolution

- **Grammar.** The ADR 014 id rules (`[A-Za-z0-9._:/-]`,
  `<namespace>/<model>`).
- **No collision at write time.** A scoped `PUT` whose id equals a current
  platform id is `LM-1001` naming it: a foundation model (disabled ones
  included), a platform virtual model or a deprecated bare alias.
- **Account first, when the platform adds an id later.** The platform is
  never checked against account documents, so staff can add any id. When a
  platform id later equals an account id, the account's model wins for that
  account's keys and the platform model stays what every other account gets.
  A team's integration therefore never changes target or capability under
  it, and the platform can never redirect a team's traffic. The account list
  reports the shadowing as a warning so the Lab can suggest a rename.
  A reserved namespace (a prefix only teams may use) was rejected: the
  current grammar leaves no prefix the platform cannot already use, so it
  would need a platform-side restriction that an existing production config
  could fail at boot.
- **Targets.** An account model may target, in this order of lookup: the
  account's own virtual models, platform virtual models, then foundation
  models. Another account's models are never visible: they are unknown ids.
- **Request path.** A key's account is the `account_ref` of its group's
  billing root (the parent of a sub-group, ADR 020). A request resolves the
  requested id in that account's compiled table first, then as before
  (deprecated alias, platform virtual model, foundation model). A key with no
  group, or a group with no `account_ref`, sees the platform only.
- **`GET /v1/models`** lists, for a key of an account, the foundation models
  and the platform's listed virtual models, minus any id the account declares,
  then the account's listed models (`"virtual": true`, `"owned_by":
  "account"`). `GET /v1/models/{id}` resolves the same way (an unlisted or
  disabled account model is a 404, never the platform model it shadows).
  Other keys see the platform list only.

### 4. Platform changes are never blocked by teams

Platform validation never reads account documents, so deleting, disabling or
changing a platform model that account models reference always succeeds.

Account tables are recompiled after every platform reload in a lenient mode:

- A target that no longer resolves (deleted foundation or platform virtual
  model, or a platform model that no longer serves the capability, or no
  longer serves `decisions` under a `remap`) becomes a **skipped leaf**: it
  produces no attempt, exactly like the ADR 014 leaf the registry no longer
  knows, so a `fallback` or `split` moves on to its other targets. When every
  attempt of a request is skipped, the request fails with `LM-2001` naming
  the first missing target.
- A model that a platform change pushes over a limit (depth or attempts, see
  5) is **disabled**: it stops resolving (`LM-2001` for its id, or a skipped
  leaf for an account model targeting it) until the account fixes it.
- Both are reported as `warnings` on the model's list entry and on its
  `/plan` (missing targets are marked `"missing": true`). A save is still
  strict: a write that references an unknown model, or breaks a limit, is
  `LM-1001`.

A disabled foundation model (ADR 014 amendment) is still in the foundation
index, so it is a valid target and is skipped at request time, as for the
platform.

### 5. Safety and cost

- **Limits per account:** at most 32 virtual models and 256 KiB of document
  JSON (`LM-1001` on a write over either), plus every ADR 014 rule. Depth and
  attempts are counted through platform virtual targets: a platform model
  reached from an account model contributes its own height and attempts to
  the ADR 014 bounds (`MAX_DEPTH` 8, `MAX_ATTEMPTS` 64).
- **No database on the request path.** Each process keeps a compiled table
  per account in memory, behind an `ArcSwap` next to the platform routing
  policy. It is refreshed: on a write, before the `204` (read your writes on
  the process that served it); after every platform reload (all accounts
  recompiled against the new platform snapshot); and by a background task
  that polls `MAX(generation)` every 5 seconds and loads only the changed
  rows, so the other processes sharing the database converge within 5
  seconds. A database error keeps the current tables. The request path adds
  one map lookup by account when the key has one.
- **Privacy.** Presets and remap text stay operator-authored text: never
  logged, never echoed in errors, never in `Debug` output. An account's
  `/plan` shows a platform virtual target as a reference (`"virtual": true,
  "platform": true`) without expanding it, so a team never sees the
  platform's presets or remaps.
- **Accounting.** Unchanged: `x-lumen-model-used`, `usage_log.model_used`,
  token counts and cost stay the foundation leaf that served; `usage_log.model`
  and `x-lumen-route` carry the account model id and route. The Prometheus
  counter `lumen_virtual_model_requests_total` uses the fixed label
  `virtual_model="account"` for account models, so team-defined ids never
  grow its cardinality.

## Consequences

- Hosted teams can manage their own virtual models through the routes the
  Lab already uses for self-hosted gateways, adding only the header.
- No new error code. `LM-1001`, `LM-1003` and `LM-1004` widen their meaning
  to account documents; the scoped routes no longer answer `LM-4005`.
- Rows of an account whose groups are all deleted stay in the table but
  resolve for no key. Purging them is a backlog item.
- A request between a platform reload and the account recompile that follows
  it may use an account table compiled against the previous platform
  snapshot. Leaves still resolve through the live registry, so the worst case
  is one request routed as it would have been just before the reload.
- Rollback: migration 0019 is additive. As for ADR 018, sqlx refuses to boot
  an older binary on a migrated database; one forced to run ignores the
  table, so account models stop resolving (`LM-2001`) and nothing else
  changes.
