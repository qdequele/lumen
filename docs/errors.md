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
| `FG-3011` | 504  | An upstream produced no first token within the deadline.       |

For `FG-3001` (and `FG-4002`/`FG-4003`), a `Retry-After` value may be advertised.

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
