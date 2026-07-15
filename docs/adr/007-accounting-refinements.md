# ADR 007 - Rate-limit and usage-log accounting refinements

- Status: accepted
- Date: 2026-07-15

## Context

M5 shipped admission control (RPM/TPM quotas, hard budgets) and the usage log
with four documented behaviours flagged in `docs/backlog.md` for revisiting
(issue #26):

1. **TPM debited the pre-call estimate and never adjusted it.** A chat request
   with `max_tokens = 2048` but 40 real output tokens permanently burned 2048
   tokens of the per-minute window, starving later requests.
2. **A request rejected by a *later* admission step still counted toward the
   earlier quotas.** `admit` bumps RPM, then TPM, then the budget CAS; when the
   budget refused (402), the RPM and TPM bumps it had already made were never
   unwound, so a refused request consumed quota it never used.
3. **The usage log recorded successful requests only.** A refusal at admission
   (402/429) errored out of `Accounting::begin` before any `UsageRecord` was
   built, so per-key rejection analytics were impossible.
4. **Metadata values were stringified** in `usage_log.metadata`
   (`{"batch":"42"}`), losing the original JSON type and blocking numeric
   filtering.

The budget already had the right shape: reserve the estimate, then
`Reservation::settle` to the real cost (over-reservation released, shortfall
charged, real cost wins even past the limit). These refinements bring TPM and
the usage log up to the same standard, without violating the hot-path rules
(no blocking, no synchronous DB write on the request path).

## Decision

### 1. TPM is settled to real usage, like the budget

`Reservation` now carries the tokens it debited to the TPM window (and the
minute it debited them in). `settle(actual_cost_micro, actual_tokens)` adjusts
the TPM window by `actual_tokens - estimate`, mirroring the budget: a smaller
real count frees the window, a larger one overshoots and the *next* request is
refused. The adjustment is a CAS loop (`adjust_window`) and is a no-op once the
window has rolled to a new minute (the debited slot has already expired).

**Asymmetry with the budget on drop.** Dropping a reservation unsettled (upstream
failure / cancellation before `settle`) refunds the *budget* (no money was
spent) but **keeps** the TPM debit. TPM is a rate limiter: a request that hit
the gateway counts against the per-minute rate even if the upstream call then
failed. Only a successful `settle`, which knows the real token count, adjusts
TPM. This preserves the established M5 principle ("refused/failed requests still
count toward the rate") while fixing the starvation caused by *over-estimates on
successful calls*, which is the common case.

### 2. Quota bumps are unwound when a later step rejects

Within a single `admit`, if the TPM step refuses after RPM was bumped, the RPM
bump is rolled back; if the budget step refuses after RPM and TPM were bumped,
both are rolled back. A request refused *inside* admission therefore consumes no
quota. (A request refused *at its own step* - e.g. RPM over the cap - never
bumped that window in the first place, since `bump_window` checks before
incrementing.) Rollback uses the same `adjust_window` CAS helper and the same
minute guard.

### 3. Rejected requests produce a status-only usage row

When admission refuses, `Accounting::begin` enqueues a `UsageRecord` with the
rejection `status` (402/429), zero tokens, zero cost, and any request metadata,
then returns the error. It goes through the *same* bounded-mpsc `UsageLogger`
as successful requests (`try_send`; a full channel drops and counts the row) -
never a synchronous DB write on the request path (the M5 hot-path rule stands).

Zero tokens is the honest count: a rejected request reached no provider and
produced nothing. The `status` column is what carries the rejection for
analytics. Scope note: 401 (unknown/invalid key) is *not* logged - it is
refused in the auth middleware before `begin`, where there is no key to
attribute the row to. Upstream failures (5xx) after admission are already
logged by the normal `finish` / `finalize_with_status` paths.

### 4. Metadata keeps its JSON value types

`RequestMetadata` stores `Vec<(String, serde_json::Value)>` (validated to
string/number/bool) instead of pre-stringified strings. `to_json` - the source
of the `usage_log.metadata` TEXT column - now emits typed JSON
(`{"batch":42,"canary":true}`), so SQLite `json_extract` can filter
numerically. Prometheus labels, which are always strings, stringify on the way
out (`label_values` returns `Cow<str>`, borrowing the string case and only
allocating for numbers/bools). The column stays TEXT; only its contents gained
type fidelity, so no migration is needed.

## Consequences

- The TPM window now tracks real usage for the common (successful) case, so
  large `max_tokens` reservations no longer starve a key for a full minute.
- Rejection analytics are possible per key (`SELECT status, COUNT(*) FROM
  usage_log ... WHERE status IN (402, 429)`), at the cost of extra usage-log
  volume during quota storms - bounded by the same channel capacity and drop
  counter as everything else.
- `Reservation::settle` gained a parameter; all call sites pass the real total
  token count (`tokens_in + tokens_out`).
- `usage_log.metadata` may now contain non-string JSON values; consumers that
  assumed all-strings should read it as typed JSON.
