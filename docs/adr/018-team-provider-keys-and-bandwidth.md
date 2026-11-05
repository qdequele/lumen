# ADR 018 - Team provider keys (BYOK) and bandwidth billing

- Status: Accepted
- Date: 2026-11-05
- Amends: 015
- Builds on: ADR 003, ADR 009, ADR 010

## Context

The hosted gateway (ADR 015) runs on Meilisearch's provider keys and bills the
provider cost. Teams want to bring their own provider keys ("bring your own
key", BYOK): the provider bills them directly, so the Lab must not charge the
token cost again. The hosted gateway then still carries their traffic, so the
Lab bills the bandwidth instead.

## Decision

1. **Pools per budget group.** On the hosted gateway one Lab account is one
   budget group, so a group owns the team keys: up to 10 keys per
   (group, provider), each with a label (1 to 64 characters), plus a
   per-provider `fallback` flag (default off). Keys are sealed at rest with
   the master key, never returned (only `label`, `last4`, status) and never
   logged. Only kinds that take a plain API key on the vendor URL are
   eligible; Azure, Bedrock, Google/Vertex and keyless kinds are not. A pool
   row is trusted only when its provider is in the live config and eligible.
   The admin API is `/admin/groups/{id}/byok` (list), `PUT .../{provider}`
   (fallback flag), `POST .../{provider}/keys`, `DELETE .../keys/{key_id}`,
   account-scoped like the other group routes. `DELETE` evicts the key from
   memory synchronously; a reload then reconciles with the store. The
   request path reads pools from memory only.
2. **Routing (`RequestKeys`).** Pools are chosen by the authenticated key's
   group, never by client input. Keys are used round-robin. A 429 benches the
   key for `max(retry_after, 30s)`; a 401/403 marks it failing and benches it
   10 minutes; either way the next key is tried. When no key is usable, the
   platform key is used if the provider's fallback is on, else the request
   fails with `LM-3021` (429, `TeamKeysExhausted`), decided by the gateway:
   never retried, not billed. With fallback off, the lock is decided up front
   from chain order: once a fallback-off team link is reached, later links
   with no team pool for their provider refuse with `LM-3021` without calling
   upstream, so a chain can never silently reach the platform's keys.
3. **Team-served requests settle non-billable.** No token cost is added to
   the settled or spent counters and the `usage_log` cost is 0. Their tokens
   are reported as `byok_tokens_in` / `byok_tokens_out`.
4. **Bandwidth metering.** A `/v1` middleware inside auth counts request bytes
   read plus response bytes written, SSE included. The response side is
   credited exactly once, on end of stream or on drop, and also when the
   client disconnects before a response exists. A request is billed only when
   an upstream call was actually sent: `UpstreamMark` is set inside the
   closures that call upstream. Gateway refusals (auth, invalid request,
   budget, RPM/TPM, team keys exhausted) are not billed.
5. **Bandwidth charge.** Bytes are charged to the key and group `spent_micro`
   at the group's `bandwidth_micro_usd_per_gb` (integer, ceil, 1 GB = 10^9
   bytes), never to the Lab billing counter `settled_micro`, so the ADR 015
   settled-cost event is not double counted. A group with a `budget_max`
   refuses when spent + reserved >= `budget_max` even for a zero token
   estimate (`LM-4001`): zero credits is a hard stop, which BYOK-only teams
   would otherwise bypass. The Lab pushes the rate as
   `floor(price / provider_cost_markup)`, so that the lease, which is in
   provider-cost space, is debited the customer price after the Lab applies
   its markup. A rate of 0 disables the charge.
6. **Usage events.** New units `bytes_in`, `bytes_out`, `byok_tokens_in`,
   `byok_tokens_out`. `tokens_in`/`tokens_out` and `provider_cost_micro_usd`
   cover platform-key traffic only. A flush window with only bandwidth or BYOK
   traffic still emits an event (provider cost 0). The vendored schema
   `contracts/vendor/lab/lab-events.schema.json` is updated to match.

## Consequences

- **Circuit breakers are scoped by who serves the link.** A link the request
  serves from a team pool (its provider has a usable pool of an eligible
  kind when the request starts) uses its own breaker, keyed
  `"{provider}#group:{group_id}"` with the same model id; every other link
  keeps the platform breaker keyed by (provider, model). Platform 429s or 5xx
  therefore never open the breaker for a team served on its own keys, a
  team's failing keys never open it for platform traffic, and a team-key
  success never closes (or takes the probe of) a breaker the platform
  opened. Keys of one pool still share their group's breaker; per-key
  breakers remain a possible follow-up. A team-scoped link that ends on the
  platform key (fallback on, every team key benched mid-request) is charged
  to the team scope. Scoped breakers stay out of the `lumen_circuit_state`
  gauge, which would otherwise grow one series per group.
- A link refused by the chain lock, or any other result that says nothing
  about the provider (a cancellation, a client fault), hands back a half-open
  probe it took, so the next request probes without waiting a cooldown.
- When the chain lock refuses a link after an earlier link of the same
  request failed upstream, the client gets that earlier error rather than
  `LM-3021`; a team link whose own pool is exhausted still answers `LM-3021`.
- Bandwidth is metered at the HTTP layer, so it counts body bytes only (no
  headers or TLS).
- Narrows ADR 015 decision 7 (out-of-scope list): "per-account provider
  keys" is now in scope for plain-key providers.

## Rollback

Migrations 0015 and 0016 are additive. sqlx refuses to boot an older binary
on a migrated database. If one is forced to run, it ignores the pools: BYOK
teams would be served on the platform's keys and billed tokens. So roll back
by disabling BYOK (the per-pool fallback flag cannot help), rather than by
downgrading the binary.
