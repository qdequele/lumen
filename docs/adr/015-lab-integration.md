# ADR 015 - Lab integration: account refs and billing usage events

- Status: accepted
- Date: 2026-09-30
- Builds on: ADR 009, ADR 010, ADR 011
- Amends: ADR 011 §2 (billing events only)
- Amended: 2026-10-10 (platform contract v2)

## Context

The Meilisearch Lab is one Rails control plane shared by Scrapix, LUMEN and
glutony: accounts, keys, one credit ledger, SSO. Data planes integrate only
through contracts, because the Lab is transitional and Meilisearch Cloud's
control plane replaces it later. Scrapix already reports usage as signed,
idempotent `usage.recorded` events to `POST {LAB_URL}/internal/events` from a
durable outbox.

LUMEN already has the control leg (ADR 010 admin push, ADR 009 groups and
atomic grants) and a best-effort reconciliation leg (`/admin/usage/export`,
fed by a `usage_log` channel that drops rows under pressure by design). It
has no billing leg that is exact.

## Decision

1. **Key push-sync.** The Lab drives `/admin` over a private network with the
   master key (ADR 010). LUMEN never calls the Lab to authorize a request.
2. **One group per Lab account per gateway is the account's lease** on a
   shared credit wallet. The Lab tops it up with `POST /admin/groups/{id}/grant`.
   Admission is unchanged.
3. **Two opaque refs:** `budget_groups.account_ref` (the Lab account id) and
   `virtual_keys.external_ref` (the Lab key id). A key is billable when its
   group has an `account_ref`.
4. **Billing rides on budget settlement, from settled cost.** Each key
   carries a settled-cost counter (the real cost added in `settle`;
   in-flight reservation estimates never touch it) and a `billed_micro`
   watermark. At every flush a billable key's event is its settled cost since
   the watermark, so a delta is never negative. It is written to
   `usage_outbox` in the same transaction that persists the spend, and the
   watermark moves to the settled cost on commit. A non-billable key's
   watermark is kept equal to its settled cost, so a key that becomes
   billable later never bills its past. Three rules keep this exact:
   - **One flush, one lock.** The periodic, shutdown and key-delete flushes
     share one function and one async lock, because the watermark moves only
     after the commit and overlapping flushes would bill a delta twice.
   - **Deleted keys are retired, not dropped.** A retired key stays in every
     flush until a flush that includes it commits and no in-flight request
     still holds it, so its last spend (even from a request still streaming)
     is billed and a failed final flush is retried.
   - **Billability changes flush first and fail closed.** With
     `[usage_events]` on, `PATCH /admin/keys/{id}` with `group_id` and
     `PATCH /admin/groups/{id}` with `account_ref` first flush pending
     spend, so it is billed to the old account. If that flush fails the PATCH
     answers 500 (`LM-5001`) and changes nothing; the caller retries.
5. **Durable delivery.** A sender pushes due outbox rows in batches, signed
   `X-Lab-Signature: sha256=<hex HMAC-SHA256>`, and retries with capped
   backoff until the Lab lists the id in `accepted`. This amends ADR 011 §2
   ("delivery state is memory-only") for billing events only: webhooks are
   signals, these events are the bill. (Amended 2026-10-09: the signature
   now covers `<timestamp>.<body>` with instance headers, and one drop rule
   applies; see the amendment below.)
6. **Units.** LUMEN reports micro-USD cost plus request and token counts. The
   Lab converts to credits. (Amended 2026-10-09: raw units plus
   `provider_cost_micro_usd`; see the amendment below.)
7. **Opt-in, boot layer.** A `[usage_events]` block enables it; without it
   the gateway is unchanged and makes no new outbound call.

## Consequences

- Billing is exactly the real cost the budget settled, with the same crash
  window (ADR 009 §4): a crash loses at most one flush of spend from both.
- The Lab must accept `product: "lumen"` and `X-Lab-Signature`, convert to
  credits, hold one secret per gateway, and run the lease top-up loop.
- Per-request detail stays in `usage_log` and the export, best-effort.
- While billing is on, a database or outbox outage blocks key moves between
  groups and `account_ref` changes (they answer 500 until the flush works).
  Serving, top-ups and everything else are unaffected.
- Residual: spend from a request admitted before such a change but settled
  after it follows the new membership.

## Alternatives considered

- **Per-request events:** requires a lossless `usage_log` path and a
  firehose on indexing bursts.
- **Lab pulls `/admin/usage/export`:** inherits drops and retention, and
  breaks the push contract the Lab already implements.
- **Per-request introspection (Scrapix model):** a network hop on the
  request path.

## Amendment 2026-10-09: platform contract v2

Decisions 2, 3 and 4 stand. Decisions 1 and 7 are extended and decisions 5
and 6 are amended by the Lab's platform contract v2 (`meilisearch/lab`,
`docs/superpowers/specs/2026-10-08-lab-platform-contract-v2.md`),
implemented in LUMEN 0.7.0:

1. **Per-instance credentials (decision 5 amended).** The gateway is a Lab
   *instance*: `LAB_URL`, `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` (first
   class in `[usage_events]` as `url`, `instance_id`, `secret_env`; the
   environment wins over the file, and `signing_key_env` stays as a
   deprecated alias with one boot warning). Every batch carries
   `X-Lab-Instance-Id`, `X-Lab-Timestamp` (unix seconds, fresh per attempt)
   and `X-Lab-Signature: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`;
   the Lab rejects a timestamp more than 300 s off. The Lab holds the secret
   per instance; "one secret per gateway" in the consequences above is now
   the Lab's own model.
2. **Self-description.** One boot-time `GET /internal/instances/me` (bearer
   secret plus `X-Lab-Instance-Id`) confirms the credentials and logs the
   identity. Engines are hosted by Meilisearch only (decision A of the
   contract), so decision 3 above is unchanged: a key is billed when its
   group has an `account_ref`. A `401` or `403`, or a `2xx` answer that is
   not this instance's identity (malformed, another `instance_id`, another
   product, a kind other than `hosted`), refuses to boot. Any other answer
   or an unreachable Lab is a warning, never a request-path concern; it
   runs after the listener is bound and before serving starts, so it delays
   serving (HTTP requests, `/health` included) by at most
   `usage_events.timeout_ms`.
3. **Units, not a price (decision 6 amended).** The event reports
   `operation: "gateway"`, `units {requests, tokens_in, tokens_out,
   tokens_estimated}` and `provider_cost_micro_usd` (the settled-cost delta).
   The lease snapshot, `source`, `key_id` and `window` leave `data`, which
   the Lab-owned schema closes; the key id and window live in
   `description` (`key <id> <start>..<end>`). The schema is vendored byte for
   byte at `contracts/vendor/lab/lab-events.schema.json`, and the CI job
   `lab-contract-drift` fails on drift from `meilisearch/lab` when the
   `LAB_REPO_TOKEN` secret is set.
4. **One drop rule (decision 5 amended).** The first `2xx` answer that keeps
   an event out of `accepted` starts a 24 h clock (`first_skipped_ms`); a
   later such answer at least 24 h after it drops the row (`dropped_ms`)
   with an error log naming the event id and
   `lumen_usage_events_dropped_total`. Non-2xx answers, timeouts and an
   unreachable Lab never start or reset the clock, so an outage still never
   drops a bill.
5. **One gateway, many accounts (decision 1 extended).** The Lab still
   drives `/admin` with the master key, but a hosted gateway is shared
   between accounts, so an `/admin/*` call carrying `X-Lumen-Account-Ref`
   is confined to that account's groups and keys (lists filtered, foreign
   ids `404 LM-1003` like unknown ones, creation forced into the account)
   and refused on the platform-only routes (provider keys and checks,
   webhooks, config, `/openapi.json`, `/health/providers`) with the new
   `403 LM-4005`. Webhooks
   are platform-only rather than filtered because the receiver is one
   gateway-wide setting. `GET /admin/groups/{id}` exposes the live
   `spent_micro` and `budget_max_micro` the Lab's lease sync reads; `PATCH`
   (`budget_max`) and `grant` set the lease.
6. **Opt-in from the environment too (decision 7 extended).** The block is
   still the only switch and still boot layer, but `LAB_URL` and
   `LAB_INSTANCE_ID` in the environment create it when the file has none
   (and overlay it when it does), so a hosted gateway is configured like
   Scrapix and glutony. Without the block and without those variables the
   gateway is unchanged and makes no new outbound call.
7. **Out of scope here:** Postgres, horizontal scaling with auth on,
   per-account provider keys (hosted LUMEN runs on Meilisearch's keys,
   decision F of the contract).

The operator view of all of this is
[Lab integration](../operations/lab-integration.md).
