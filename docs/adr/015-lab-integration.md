# ADR 015 - Lab integration: account refs and billing usage events

- Status: accepted
- Date: 2026-09-30
- Builds on: ADR 009, ADR 010, ADR 011
- Amends: ADR 011 §2 (billing events only)

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
   signals, these events are the bill.
6. **Units.** LUMEN reports micro-USD cost plus request and token counts. The
   Lab converts to credits.
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
