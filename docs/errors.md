# Error codes

Every error Ferrogate returns to a client carries a stable `FG-XXXX` code, an
HTTP status, and a coarse `type`. The response body is always:

```json
{ "error": { "code": "FG-1001", "message": "…", "type": "invalid_request" } }
```

The `type` is one of `invalid_request`, `upstream_error`, or `internal`. The
gateway always distinguishes three situations and never disguises one as
another — in particular, an internal malfunction is never reported as a
misleading `401` (a lesson from OpenRouter outages).

Codes are stable: once assigned, a code keeps its meaning across releases.

## Client errors — `FG-1xxx` · `type: invalid_request`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-1001` | 400  | Malformed or invalid request body / parameters.                |
| `FG-1002` | 404  | The requested model id was not found.                          |
| `FG-1003` | 400  | The model exists but does not serve the requested capability.  |
| `FG-1004` | 413  | Request body exceeded the configured size limit.               |
| `FG-1005` | 401  | Missing or invalid virtual key. *(M5)*                          |
| `FG-1006` | 402  | The virtual key's hard budget is exhausted. *(M5)*             |
| `FG-1007` | 429  | A gateway-side quota (RPM/TPM) was exceeded. *(M5)*            |

## Upstream errors — `FG-2xxx` · `type: upstream_error`

These always name the provider that failed. Retriable ones may be transparently
retried on a fallback (M6) before surfacing.

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-2001` | 502  | An upstream provider returned an error status.                 |
| `FG-2002` | 503  | No healthy upstream available (circuit open / fallbacks spent).|
| `FG-2003` | 504  | An upstream provider timed out.                                |
| `FG-2004` | 429  | An upstream provider rate limited the request.                 |

For `FG-2004` (and `FG-1007`), a `Retry-After` value may be advertised.

## Internal errors — `FG-5xxx` · `type: internal`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `FG-5001` | 500  | Internal gateway malfunction.                                  |

Internal errors return an opaque `"internal error"` message to the client; the
underlying detail is written only to the server logs, never the response.

> Codes marked *(M5)* / *(M6)* are defined now (the taxonomy is stable) but are
> only emitted once the corresponding milestone lands.
