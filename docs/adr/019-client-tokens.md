# ADR 019 - Client tokens for browser and mobile callers

- Status: proposed
- Date: 2026-10-07
- Builds on: ADR 008, ADR 009
- Required by: ADR 018 (live transcription from browsers)

## Context

Every LUMEN credential today is a virtual key (`fg-` plus 32 random bytes)
meant to live on a backend. Browser and mobile front ends now need to call
LUMEN directly, first to stream microphone audio into a live transcription
session (ADR 018), then to upload a recording. Two things block that:

- **A virtual key in a front end is public.** Anyone can copy it and spend
  its whole budget on every model.
- **Browsers cannot set `Authorization` on a WebSocket.** The only things a
  page controls during the handshake are the URL and the
  `Sec-WebSocket-Protocol` header.

The providers solved this the same way. OpenAI's
`POST /v1/realtime/client_secrets` returns a short-lived `ek_` secret,
usable for several sessions until it expires, with the session config
attached; browsers pass it as the WebSocket subprotocol
`openai-insecure-api-key.<secret>`. A session outlives the secret once it has
started. ElevenLabs mints single-use tokens that go in a `token` query
parameter. In both, a backend that holds the real key mints the short-lived
credential and the front end only ever sees that.

LUMEN's constraints: no database on the request path (pillar 3), secrets
never logged (rule 5), and a fleet with no shared state yet.

## Decision

### 1. Minting: `POST /v1/realtime/client_secrets`

OpenAI's path and body, so OpenAI SDKs and examples work against LUMEN:

```json
{
  "expires_after": { "anchor": "created_at", "seconds": 60 },
  "session": {
    "type": "transcription",
    "audio": { "input": { "transcription": { "model": "gpt-live-transcribe" } } }
  },
  "lumen": {
    "capabilities": ["transcribe_live", "transcribe"],
    "models": ["gpt-live-transcribe", "scribe_v2"],
    "max_session_seconds": 900,
    "max_cost_usd": 0.50,
    "origins": ["https://app.example.com"]
  }
}
```

- Authenticated by a **virtual key only**. The master key cannot mint (a
  token needs a parent key whose budget it spends), and a client token cannot
  mint another one (no chaining).
- `expires_after.seconds` is the connect window: 10 to `max_ttl_seconds`
  (default 60, cap 600). As with OpenAI, a session or upload started before
  expiry runs to completion.
- `session` is optional. When present it must be a transcription session; its
  model is added to the token's model list and the session config becomes
  the default applied when the client connects.
- The optional `lumen` object (`extra_body` in the OpenAI SDKs) narrows the
  token: `capabilities` (default `["transcribe_live"]`; v1 accepts
  `transcribe` and `transcribe_live` only), `models` (at most 8; default the
  session model, required when there is no session), `max_session_seconds`
  (at most the gateway cap), `max_cost_usd` (per session or per upload),
  `origins` (a subset of the gateway allowlist).
- The response is OpenAI's shape: `{ "value": "fgt-...", "expires_at": ...,
  "session": {...} }`.
- Minting counts as one request against the parent key's RPM. It costs no
  budget.

A token can never do more than its parent key: same models or fewer, never
admin routes, always charged to the parent's budget, quotas and group (ADR
009). Virtual keys have no model allowlist today, so the token's model list
is the first model scoping LUMEN has. It is only on tokens.

### 2. Format: a signed, self-contained token

`fgt-` + base64url(claims) + `.` + base64url(MAC)

- **Claims**, compact JSON: `v` (format version), `kid` (signing key id),
  `key` (the parent key's id, never its hash or plaintext), `jti` (16 random
  bytes), `iat`, `exp`, `caps`, `models`, `max_s`, `max_micro`, `origins`.
  Bounded to 1 KiB, well under header limits.
- **MAC**: BLAKE3 in keyed mode over `fgt-` plus the claims segment, with a
  32-byte signing secret. BLAKE3 is already in `crates/auth` for key hashing,
  verification is well under a microsecond, and the comparison is constant
  time.
- **Signing secret**: `[client_tokens] signing_secret`, read from the
  environment like `master_key`, at least 32 bytes, never logged, redacted in
  `Debug`. Rotation: set the new secret and move the old one to
  `previous_signing_secret`; tokens carry `kid`, so both verify until the old
  tokens expire (at most `max_ttl_seconds`), then the previous secret is
  removed. Both fields hot-reload (ADR 008).

### 3. Verification, with no database

On every request or upgrade carrying an `fgt-` credential: split, check the
MAC against the `kid`'s secret, check `exp` against the clock (5 s skew
allowed), then look the parent key up in the in-memory key table that
virtual-key auth already uses. A disabled, expired or deleted parent rejects
the token on the spot: revoking the key revokes its tokens with no extra
list. Then the scope check: route capability, model, origin. All in memory.

- Bad MAC, unknown `kid`, expired token or dead parent: `LM-4004`, as vague as
  for a bad virtual key, so callers cannot probe which one it was.
- Valid token used outside its scope (capability, model, a session longer
  than `max_s`, a session that would exceed `max_micro`): `LM-4007` (403).

### 4. Where a token is accepted

- **HTTP:** `Authorization: Bearer fgt-...`, on routes whose capability the
  token carries (`POST /v1/audio/transcriptions` in v1).
- **WebSocket:** `Sec-WebSocket-Protocol: realtime, openai-insecure-api-key.fgt-...`
  (OpenAI's browser convention, so their client code works unchanged). LUMEN
  echoes only `realtime` back, never the token.
- **Query parameter** `?token=fgt-...`: off by default
  (`allow_query_token = false`), for clients that cannot set a subprotocol.
  When enabled, LUMEN strips it from its own logs and traces. Operators are
  told that proxies in front of LUMEN may still record it.

**Virtual keys from browsers.** A request that carries an `Origin` header and
authenticates with an `fg-` virtual key is refused with `LM-4008`
(`reject_browser_keys = true`, the default), so a key pasted into front-end
code fails right away in development instead of leaking in production.
Server-side callers send no `Origin` and are unaffected.

### 5. Origins and CORS

- `[client_tokens] allowed_origins` is a list of exact origins
  (`https://app.example.com`) plus single-label wildcards
  (`https://*.example.com`). It must be non-empty for the feature to start.
- **WebSocket upgrades** with a client token check the `Origin` header
  against the allowlist and the token's `origins`; a mismatch is `LM-4008`
  (403) before the upgrade. A missing `Origin` (a mobile app or a server)
  passes: the check protects browsers against cross-site WebSocket
  hijacking, which needs a browser.
- **CORS** (tower-http `cors` feature) is enabled only on the routes a client
  token can reach, only for allowlisted origins, allowing `Authorization` and
  `Content-Type`, without credentials. Admin routes and every other `/v1`
  route send no CORS headers, as today.

### 6. Accounting

Usage, cost, quotas and budget go to the parent key, unchanged. `usage_log`
gains a nullable `client_token_id` column (the `jti`, which is random and not
a secret), so an operator can trace spend to one minted token. Prometheus
gains `lumen_client_tokens_minted_total` and
`lumen_client_token_rejections_total{reason}` with a fixed `reason` set
(`invalid`, `scope`, `origin`, `browser_key`).

### 7. Opt-in

Without a `[client_tokens]` block, the minting route answers `LM-1003`
(404), no `fgt-` credential is parsed, no CORS header is sent, and
`reject_browser_keys` is off. Nothing changes for existing deployments.

### 8. New error codes

| Code | HTTP | Meaning |
|---|---|---|
| `LM-4007` | 403 | A valid client token was used outside its scope: capability, model, session length or per-session cost cap. |
| `LM-4008` | 403 | Origin not allowed for this client token, or a virtual key was sent from a browser (`Origin` present) while `reject_browser_keys` is on. |

## Consequences

- A front end gets transcription with no backend proxy for audio, and the
  operator's key never leaves the backend.
- A leaked token is usable by anyone until it expires (60 s by default), but
  only within its models, session length and per-session cost cap, from
  allowed origins in a browser, and against the parent key's budget and
  session limits (ADR 018). Revoking the parent key stops it immediately.
- Tokens are multi-use until expiry, like OpenAI's. `max_cost_usd` caps each
  session or upload, not the total across a token's sessions; the per-key
  session limit and the short TTL bound the total.
- The capability list makes the mechanism reusable. Allowing chat or
  embeddings from browsers later is a change to the accepted list plus a
  review, not a new design.
- One more secret to provision and rotate per gateway, shared by every
  instance in a fleet.

## Alternatives considered

- **JWT via `jsonwebtoken`.** Interoperable, but no third party needs to read
  these tokens, and JWT brings algorithm-confusion pitfalls and a larger
  token for no benefit. Rejected for a fixed-format BLAKE3 MAC.
- **Opaque random tokens stored in SQLite.** Trivial revocation and real
  single use, but a database read on every connection (pillar 3). Rejected.
- **Single-use tokens.** Stops replay inside the TTL, but enforcing it across
  a fleet needs shared state LUMEN does not have yet (Postgres or Redis,
  `docs/backlog.md`). A per-instance version would promise more than it
  delivers. Deferred until fleet state exists. Multi-use with a short TTL
  matches OpenAI.
- **Pass through provider tokens** (OpenAI `ek_`, ElevenLabs single-use).
  The browser would talk to the provider directly and skip LUMEN's budgets,
  quotas, accounting and fallback. Rejected.
- **WebRTC for browsers**, which OpenAI recommends for browser audio.
  Robust on poor networks, but it brings ICE, DTLS and SRTP into a gateway
  meant to stay a small single binary. Rejected for v1; WebSocket with a
  client token covers the need.
