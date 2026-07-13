# Error codes

Every error Ferrogate returns to a client carries a stable `FG-XXXX` code, an
HTTP status, and a coarse `type`. The response body is always:

```json
{ "error": { "code": "FG-1001", "message": "…", "type": "invalid_request" } }
```

The `type` is one of `invalid_request`, `upstream_error`, or `internal`. The
gateway always distinguishes three situations and never disguises one as
another — in particular, an internal malfunction is never reported as a
misleading `401` (a lesson from OpenRouter outages), and a malformed *upstream*
response is a `502`, never a gateway `500`.

Codes are stable: once assigned, a code keeps its meaning across releases. The
code prefix groups by cause: `1xxx` request, `2xxx` routing, `3xxx` upstream,
`4xxx` auth/budget, `5xxx` internal.

## Request errors — `FG-1xxx` · `type: invalid_request`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-1001` | 400  | Malformed or invalid request body / parameters.                |
| `FG-1002` | 413  | Request body exceeded the configured size limit.               |

## Routing & capability-request errors — `FG-2xxx` · `type: invalid_request`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-2001` | 404  | The requested model id was not found.                          |
| `FG-2002` | 400  | The model exists but does not serve the requested capability.  |
| `FG-2010` | 400  | A rerank request supplied no `documents` to score.             |

## Upstream errors — `FG-3xxx` · `type: upstream_error`

These always name the provider that failed. Retriable ones may be transparently
retried on a fallback (M6) before surfacing.

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-3001` | 429  | An upstream provider rate limited the request.                 |
| `FG-3002` | 502  | An upstream provider returned an unparseable/malformed response.|
| `FG-3003` | 502  | An upstream provider returned an error status.                 |
| `FG-3004` | 503  | No healthy upstream available (circuit open / fallbacks spent).|
| `FG-3005` | 504  | An upstream provider timed out.                                |
| `FG-3010` | 502  | An upstream stream ended prematurely (no terminator).          |
| `FG-3011` | 504  | An upstream produced no first token within the first-token deadline. |
| `FG-3012` | 504  | The connection to an upstream could not be established within the connect timeout. |
| `FG-3013` | 504  | The whole request (all retries + fallbacks) exceeded the total timeout. |
| `FG-3020` | 503  | The provider's circuit breaker is open and no fallback remained. |

For `FG-3001`, `FG-3020` (and `FG-4002`/`FG-4003`), a `Retry-After` value may be
advertised. The three timeouts (`FG-3011` first-token, `FG-3012` connect,
`FG-3013` total) are distinct codes purely for debugging — see M6 §6.4 and
`docs/adr/005-resilience-execution.md`.

### How resilience shapes these codes (M6)

The `3xxx` codes are what a client sees only *after* the resilience machinery
has given up. Before surfacing, a retryable failure (`FG-3001` 429,
`FG-3003` 5xx, `FG-3005`/`FG-3012` timeouts) is retried with exponential
backoff, then the request fails over to the model's configured `fallbacks`.
The mapping between a failure and the code that eventually surfaces:

- **`FG-3020` (503)** — the primary's circuit is open and no fallback remained.
  Skipping an open circuit is instant (no upstream call), and the response
  carries a `Retry-After` equal to the cooldown remainder.
- **`FG-3004` (503)** — every link in the fallback chain was tried and failed
  (retries exhausted or circuits open all the way down).
- **`FG-3013` (504)** — the total per-request deadline elapsed while retrying or
  failing over; it bounds *all* attempts together, so a slow chain fails here
  rather than hanging.
- **`FG-3011` / `FG-3012` (504)** — first-token and connect timeouts; each is a
  retryable failure on its own before it surfaces.

A hard upstream client error (a 4xx bad request) is **never** retried or failed
over — a different provider would reject it too — and surfaces immediately.
Whichever model ultimately served a successful request is reported in the
`x-ferrogate-model-used` response header.

## Auth / budget errors — `FG-4xxx` · `type: invalid_request`

Codes pinned by the M5 spec. Enforcement happens in memory, **before** any
upstream call — a rejected request never leaks spend to a provider.

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-4001` | 402  | The virtual key's hard budget is exhausted.                    |
| `FG-4002` | 429  | The key's requests-per-minute quota was exceeded.              |
| `FG-4003` | 429  | The key's tokens-per-minute quota was exceeded.                |
| `FG-4004` | 401  | Missing or invalid virtual key. Deliberately does not say *why* (unknown, disabled and expired are indistinguishable) so callers cannot probe key state. |

## Internal errors — `FG-5xxx` · `type: internal`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-5001` | 500  | Internal gateway malfunction.                                  |

Internal errors return an opaque `"internal error"` message to the client; the
underlying detail is written only to the server logs, never the response.
