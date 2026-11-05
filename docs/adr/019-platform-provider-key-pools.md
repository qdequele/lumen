# ADR 019 - Platform provider key pools

- Status: Accepted
- Date: 2026-11-05
- Builds on: ADR 018

## Context

On the hosted gateway every team that does not bring its own keys (ADR 018)
is served with the platform's provider keys, Meilisearch's own. Each provider
has exactly one: the `api_key_env` variable of its config entry, or the key
stored with `PUT /admin/provider-keys/{name}` when the variable is unset.
Changing it means a deploy (env) or a raw admin call, and a rate limit on
that one key fails every customer of the provider: there is no second key to
rotate to.

ADR 018 already built what is missing, for teams: a pool of sealed keys per
provider, round-robin, benched on a rate limit or a rejection, with a live
status per key. The Lab wants Meilisearch staff to manage the platform's keys
the same way from the console.

## Decision

1. **A platform pool per provider.** A new table `platform_provider_keys`
   holds up to 20 keys per provider, each with a label (1 to 64 characters),
   `last4`, an `enabled` flag and the key sealed with the master key
   (AES-256-GCM, like `provider_keys` and team keys). Keys are never returned
   and never logged; eligibility is the ADR 018 one (plain API key kinds
   only). Deleting a provider from the config deletes its platform keys too.
2. **Admin API, platform-only.** `GET /admin/platform-keys` lists every
   provider of the config document with its pool (`id`, `label`, `last4`,
   `enabled`, live `status`) and whether the provider also has a configured
   key (`configured_key`: env or `provider_keys`). `POST
   /admin/platform-keys/{provider}/keys` adds a key, `PATCH
   .../keys/{key_id}` turns it on or off, `DELETE .../keys/{key_id}` removes
   it. They sit with the platform routes: master key only, and a call scoped
   with `X-Lumen-Account-Ref` is refused `403 LM-4005`, so a team can never
   reach them through the Lab proxy. Disable and delete evict the key from
   memory before answering; the reloader then reconciles with the store.
3. **Routing.** Wherever the platform key served a request before (no team
   pool for the provider, or a team pool with the fallback on and no usable
   team key), the platform pool is tried first with the ADR 018 rotation
   rules: round-robin, a 429 benches the key `max(retry_after, 30s)`, a
   401/403 marks it failing for 10 minutes, and the next key is tried. When
   no pool key is usable the configured key serves the request, if the
   provider has one; otherwise the request ends with the last pool error
   (the provider's 429 or 401/403), as a lone platform key would have.
   Disabled keys are not in the pool. A provider with no platform pool
   behaves exactly as before.
4. **The fallback-off guarantee is unchanged.** The platform pool sits
   behind the same branch as the configured key, so a team with the fallback
   off never reaches it: `LM-3021` and the chain lock (ADR 018 decision 2)
   apply to the platform pool exactly as to the configured key.
5. **Billing and breakers.** A request served by a platform pool key is a
   platform request: `Served::Platform`, tokens billed, platform circuit
   breaker. Rotation inside the pool happens before the breaker sees the
   attempt's outcome, so one rate-limited key does not open the breaker
   while another key of the pool succeeds.

## Consequences

- Staff rotate, add and disable platform keys at runtime; no deploy. The
  env key keeps working as the last resort, and can be dropped from the
  deployment once a pool is in place.
- A platform request may make several upstream calls (one per benched key)
  before it succeeds, as team requests already do.
- Key status is per gateway process and in memory (`unused` after a restart),
  as for team keys.
- Audit: each write logs (`tracing::info!`) the provider and key id, never
  the secret. The control plane (the Lab) keeps the who-did-what audit trail.
