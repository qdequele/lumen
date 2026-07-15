# ADR 006 - A dedicated error code for client-initiated cancellation

- Status: accepted
- Date: 2026-07-15

## Context

`ProviderError::Cancelled` is produced when the per-request `CancellationToken`
fires - normally because the client disconnected mid-request (ADR 004, M6).
Since M1 this mapped to `GatewayError::Internal("request cancelled")`: HTTP
500, `type: internal`, `LM-5001`.

That mapping was flagged in `docs/backlog.md` at M1 ("revisit in M4") and
tracked as GitHub issue #11: a client hanging up is not a gateway
malfunction, but reporting it as `LM-5001`/500 makes it indistinguishable
from one in every place that matters operationally - the
`lumen_http_request_duration_seconds{status}` / `lumen_request_duration_seconds{status}`
Prometheus histograms, and any alert rule built on the `5xx` or `status="500"`
label. A busy gateway with many client cancels (slow clients, mobile
networks, users navigating away mid-stream) would then look, to an operator,
identical to a gateway that is actually failing.

The existing taxonomy (`CLAUDE.md` rule 8, `docs/errors.md`) is pinned to
exactly three situations - client error / upstream error / internal error -
each with its own `type`. A client cancel does not fit any of the three:
it is not a rejected request (the request was valid), not an upstream fault
(the provider never got a chance to fail), and not a gateway malfunction (the
gateway did exactly what it should: stop work for a client that left).
Silently picking one of the three to avoid extending the enum would keep
reproducing the same misclassification this issue exists to fix.

## Decision

### A fourth `ErrorType`, `LM-6xxx` as its own code prefix

Add `GatewayError::ClientCancelled` (`LM-6001`) and `ErrorType::ClientCancelled`
(serialized `"client_cancelled"`), alongside - not replacing - the existing
three-way split. `docs/errors.md` documents it as its own section, and the
code-prefix table gains `6xxx` for client-cancellation. This is an additive
change to the public envelope schema (`type` gains a fourth possible value);
existing clients that switch on the three known values are unaffected as long
as they have a default case, which the taxonomy already requires them to
handle (new codes are added within existing prefixes routinely).

`ProviderError::Cancelled` now maps to `GatewayError::ClientCancelled` instead
of `GatewayError::Internal` in `GatewayError::from_provider`.

### HTTP status: 499

`499` is the conventional "client closed request" status popularised by
nginx - not in the IANA registry, but widely recognised in logs/dashboards
and, critically, **not a 5xx**. The client has normally already disconnected
by the time this status would be written, so it is never actually read by
anyone; its only audience is server logs and the `status` label on the
latency histograms. Any other 4xx would misleadingly imply the *client's
request* was at fault (it wasn't - the request was fine, the client just left
before it finished).

### Telemetry: an explicit `"499"` label, not the `"4xx"` catch-all

`crates/telemetry/src/latency.rs::status_str` already degrades uncommon
statuses to a coarse class (`"4xx"`, `"5xx"`, ...) to keep Prometheus
cardinality bounded. `499` gets an explicit arm instead, ahead of that
catch-all, for the same reason `LM-6001` gets its own code rather than being
folded into an existing one: an operator dashboarding cancellation volume
(e.g. to catch a client-side bug causing excessive aborts) needs to see it
distinctly from ordinary `4xx` client errors, not just confirm it isn't a
`5xx`.

## Consequences

- `lumen_http_request_duration_seconds{status="499"}` and
  `lumen_request_duration_seconds{status="499"}` now separate client cancels
  from both `5xx` internal failures and generic `4xx` client errors. Existing
  alert rules built on `status=~"5.."` automatically stop firing on client
  cancels - no rule changes required, which was the point of the issue.
- The public error envelope's `type` field gains a fourth possible value,
  `client_cancelled`. This is additive; `docs/errors.md` is updated as the
  source of truth.
- `crates/core/src/error.rs`'s three-way taxonomy doc comment (CLAUDE.md rule
  8) now notes this fourth, orthogonal situation rather than silently
  breaking the "always exactly three" framing.
