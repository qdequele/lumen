# Error codes

Every error LUMEN returns to a client carries a stable `LM-XXXX` code, an
HTTP status, and a coarse `type`. The response body is always:

```json
{ "error": { "code": "LM-1001", "message": "…", "type": "invalid_request" } }
```

The `type` is one of `invalid_request`, `upstream_error`, `internal`, or
`client_cancelled`. The gateway always distinguishes these situations and
never disguises one as another - in particular, an internal malfunction is
never reported as a misleading `401` (a lesson from OpenRouter outages), a
malformed *upstream* response is a `502`, never a gateway `500`, and a
client-initiated cancel (`client_cancelled`) is never counted as an internal
malfunction either (see `LM-6xxx` below).

Codes are stable: once assigned, a code keeps its meaning across releases. The
code prefix groups by cause: `1xxx` request, `2xxx` routing, `3xxx` upstream,
`4xxx` auth/budget, `5xxx` internal, `6xxx` client-cancellation.

## Request errors - `LM-1xxx` · `type: invalid_request`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-1001` | 400  | Malformed or invalid request body / parameters.                |
| `LM-1002` | 413  | Request body exceeded the configured size limit.               |

## Routing & capability-request errors - `LM-2xxx` · `type: invalid_request`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-2001` | 404  | The requested model id was not found.                          |
| `LM-2002` | 400  | The model exists but does not serve the requested capability.  |
| `LM-2003` | 400  | An image content part was sent to a model without the `image` modality (chat vision M8 and embeddings M9). |
| `LM-2004` | 400  | A remote image URL was sent to a provider that only accepts inline base64 image data (chat vision M8). Checked before any upstream call for the primary route; if a fail-over reaches an image-incapable fallback further down the chain, the same code and status surface there too, naming the fallback provider - never the generic `LM-3002` a translation failure would otherwise produce (GH #13). |
| `LM-2005` | 400  | A remote image URL was supplied to `/v1/embeddings` but server-side image fetching is disabled (`[image_fetch] enabled = false`). Inline the image as a `data:` URI or enable fetching (M9). |
| `LM-2006` | 400  | A remote image URL was rejected by a fetch guard (scheme, host/prefix allowlist, private-IP block, size cap, per-request count cap, or non-image content type). The specific reason is logged server-side, never returned (M9). |
| `LM-2007` | 502  | A permitted image fetch failed at the remote host (network error, timeout, or error status). `type: upstream_error` (M9). |
| `LM-2008` | 400  | A provider-native image source (Anthropic `file_id`, spelled `anthropic-file:<id>`; Gemini `fileUri`, a `gs://` GCS URI or a Gemini Files API URI) was sent to a provider that cannot resolve it - the resolved primary provider must match the reference's own provider. |
| `LM-2010` | 400  | A rerank request supplied no `documents` to score.             |

## Upstream errors - `LM-3xxx` · `type: upstream_error`

These always name the provider that failed. Retriable ones may be transparently
retried on a fallback before surfacing.

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-3001` | 429  | An upstream provider rate limited the request.                 |
| `LM-3002` | 502  | An upstream provider returned an unparseable/malformed response.|
| `LM-3003` | 502  | An upstream provider returned an error status.                 |
| `LM-3004` | 503  | No healthy upstream available (circuit open / fallbacks spent).|
| `LM-3005` | 504  | An upstream provider timed out.                                |
| `LM-3010` | 502  | An upstream stream ended prematurely (no terminator).          |
| `LM-3011` | 504  | An upstream produced no first token within the first-token deadline. |
| `LM-3012` | 504  | The connection to an upstream could not be established within the connect timeout. |
| `LM-3013` | 504  | The whole request (all retries + fallbacks) exceeded the total timeout. |
| `LM-3020` | 503  | The provider's circuit breaker is open and no fallback remained. |

For `LM-3001`, `LM-3020` (and `LM-4002`/`LM-4003`), a `Retry-After` value may be
advertised. The three timeouts (`LM-3011` first-token, `LM-3012` connect,
`LM-3013` total) are distinct codes purely for debugging - see §6.4 and
`docs/adr/005-resilience-execution.md`.

### How resilience shapes these codes

The `3xxx` codes are what a client sees only *after* the resilience machinery
has given up. Before surfacing, a retryable failure (`LM-3001` 429,
`LM-3003` 5xx, `LM-3005`/`LM-3012` timeouts) is retried with exponential
backoff, then the request fails over to the model's configured `fallbacks`.
The mapping between a failure and the code that eventually surfaces:

- **`LM-3020` (503)** - the primary's circuit is open and no fallback remained.
  Skipping an open circuit is instant (no upstream call), and the response
  carries a `Retry-After` equal to the cooldown remainder.
- **`LM-3004` (503)** - every link in the fallback chain was tried and failed
  (retries exhausted or circuits open all the way down).
- **`LM-3013` (504)** - the total per-request deadline elapsed while retrying or
  failing over; it bounds *all* attempts together, so a slow chain fails here
  rather than hanging.
- **`LM-3011` / `LM-3012` (504)** - first-token and connect timeouts; each is a
  retryable failure on its own before it surfaces.

A hard upstream client error (a 4xx bad request) is **never** retried or failed
over - a different provider would reject it too - and surfaces immediately.
Whichever model ultimately served a successful request is reported in the
`x-lumen-model-used` response header.

## Auth / budget errors - `LM-4xxx` · `type: invalid_request`

Codes pinned by the spec. Enforcement happens in memory, **before** any
upstream call - a rejected request never leaks spend to a provider.

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-4001` | 402  | The virtual key's hard budget is exhausted.                    |
| `LM-4002` | 429  | The key's requests-per-minute quota was exceeded.              |
| `LM-4003` | 429  | The key's tokens-per-minute quota was exceeded.                |
| `LM-4004` | 401  | Missing or invalid virtual key. Deliberately does not say *why* (unknown, disabled and expired are indistinguishable) so callers cannot probe key state. |

## Internal errors - `LM-5xxx` · `type: internal`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-5001` | 500  | Internal gateway malfunction.                                  |

Internal errors return an opaque `"internal error"` message to the client; the
underlying detail is written only to the server logs, never the response.

## Client-cancellation - `LM-6xxx` · `type: client_cancelled`

| Code      | HTTP | Meaning                                                        |
|-----------|------|----------------------------------------------------------------|
| `LM-6001` | 499  | The client disconnected before the request completed; the upstream call was aborted. |

`499` is the conventional "client closed request" status (nginx). The client
is normally already gone by the time this would be returned, so the status
exists for logs and metrics, not for anything a client reads. It is
deliberately kept out of both `type: internal` and the `5xx` status class: a
client hanging up is not a gateway malfunction, and must never inflate the
`internal`-error metrics or alerts a real one would (issue #11, see
`docs/adr/006-client-cancellation-error-code.md`).

Two paths produce it. A cancellation surfacing mid-stream is emitted as a
terminal SSE error frame carrying this envelope. A client that simply
disconnects mid-stream never sees a frame at all, but the request's
accounting record (`usage_log.status` and the
`lumen_request_duration_seconds{status="499"}` sample) is settled at 499
instead of being miscounted as a 200 success. A non-streaming disconnect
drops the request before any outcome is recorded and produces no sample.
