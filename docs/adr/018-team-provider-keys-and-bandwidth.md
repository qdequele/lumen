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

- **Circuit breakers stay shared** per (provider, model) across team and
  platform traffic. A team's failing key can open a breaker that also affects
  platform traffic for that model, and the reverse. Accepted as a known
  trade-off; per-key breakers are a possible follow-up.
- Bandwidth is metered at the HTTP layer, so it includes headers-free body
  bytes only, not TLS or header overhead.
- Supersedes decision 7 of ADR 015 ("per-account provider keys" out of
  scope): it is now in scope for plain-key providers.
