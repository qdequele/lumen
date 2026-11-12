# ADR 023 - Cursor pagination for the admin key and group listings

- Status: accepted
- Date: 2026-11-12
- Relates to: [ADR 010](010-operator-console-and-fleet-control.md) (usage
  export cursor), [ADR 015](015-lab-integration.md) (the Lab vendors this
  contract)

## Context

`GET /admin/keys` and `GET /admin/groups` return every matching row in one
JSON array. That was fine for a handful of keys; on the hosted gateway the
number of keys and groups grows with the number of Lab accounts, and one call
serialises the whole table.

Two constraints shape the fix:

- The Lab vendors `docs/openapi.yaml` and already calls both routes. Its
  client reads the body as a bare array and expects every row. A change of
  body shape, or a default page size, would silently truncate what it sees
  until it is redeployed.
- A listing is read while keys are created and deleted. `OFFSET` pagination
  skips or repeats rows under concurrent writes and reads every skipped row.

## Decision

Pagination is opt-in and strictly additive.

- **Two optional query parameters**, `limit` (1 to 1000) and `cursor`.
  With neither, the response is exactly what it was: the whole listing, no
  extra header. With either, the response is one page. `cursor` without
  `limit` uses a page size of 100.
- **The body does not change.** A page is the same bare array of the same
  records. The continuation travels in a response header, `X-Next-Cursor`,
  present while more rows remain and absent on the last page. The store
  reads `limit + 1` rows to know, so a page that ends the listing never
  announces an empty next page.
- **Keyset, not offset.** Pages are ordered by `(created_at, id)`, the order
  the unpaginated listing already has, so the pages of a walk concatenate to
  it. Both columns are written once at creation and never updated. The next
  page is `WHERE (created_at, id) > (?, ?) ... LIMIT ?`, a seek on the
  `(created_at, id)` indexes that migration 0021 adds to `virtual_keys` and
  `budget_groups`. Every row that exists when a walk starts is returned
  exactly once (unless it is deleted meanwhile): a deleted row cannot shift
  the others. A row created during a walk sorts after the cursor and is
  picked up later, with one exception: `created_at` has one-second
  resolution and ids are random, so a row created in the same second as the
  last row already returned can sort before the cursor and wait for the next
  walk. It is never returned twice.
- **The cursor is opaque**: lowercase hex of `<created_at>:<id>` of the last
  row of the page. Hex is safe unescaped in a query string and a header.
  Clients must send it back unchanged; the encoding is not part of the
  contract. It carries no filter and no scope: the caller repeats its query
  parameters and `X-Lumen-Account-Ref` on each page, and the account scope
  is re-applied on every call, so a cursor cannot widen what a scoped caller
  sees.
- **Invalid input is refused, not corrected**: `limit` of 0 or above 1000,
  and a cursor that does not decode, are 400 `LM-1001`. As for the usage
  export (ADR 010), a silent clamp would read as "that was everything".

### Why the default stays unpaginated

A default page size would be the cleaner API in isolation, and it is what a
new route would get. Here it would be a breaking change with a quiet failure
mode: an existing client that never looks at `X-Next-Cursor` would receive
the first page, treat it as the full set, and (for the Lab's reconciliation)
conclude that the keys beyond it no longer exist. An over-large response is a
visible, bounded cost; a truncated one is silent data loss. Clients that need
bounded responses opt in with `limit`.

### Scope

Only the two listings backed by tables that grow without bound. The team and
platform provider-key listings are capped by construction (10 keys per
group and provider, 20 per provider), the config listings (providers,
virtual models) return an object read from one document, and
`GET /admin/usage/export` already has its own id cursor in the body.

## Consequences

- Existing clients see no change. A client that adopts `limit` must follow
  `X-Next-Cursor` until it is absent.
- Two more indexes are maintained on key and group creation (never on the
  request path of `/v1/*`: admission reads the in-memory state).
- A filtered page (`external_ref`, `account_ref`, `parent_id`, the account
  scope) may still be answered through the filter's own index followed by a
  sort of the matching rows; that set is one account's or one ref's rows,
  not the table.
- The cursor is tied to `(created_at, id)`. Changing the listing order later
  means a new cursor format; old cursors would then be refused with
  `LM-1001`, which a client handles by restarting the walk.
