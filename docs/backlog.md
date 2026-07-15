# Backlog

Ideas surfaced during development that are intentionally out of scope for v1
(see `CLAUDE.md` → "What we do NOT do (v1)" and `ROADMAP.md` → "Backlog v2").
Recorded here so they are not lost, and so we don't gold-plate the current
milestone.

## Deferred to v2 (from the vision)

- Web admin UI
- Semantic cache
- Multimodal (images / audio) support
- Guardrails / moderation
- Distributed rate limiting (Redis)
- OTLP tracing export
- WASM plugin system

## Noted while building M1

- Token-array inputs for `/v1/embeddings` (`input` as arrays of token ids) are
  not modelled - only string and string-batch. Add if a provider needs it.
- Rerank `documents` accepts only strings; Cohere also allows objects. Reduce
  object documents to text at the edge when a provider requires it.
- Error taxonomy (revisit in M4): `ProviderError::Cancelled` currently maps to
  `GatewayError::Internal` (500 / `internal`). Once real streaming/provider
  calls exist, a client-initiated cancel should not inflate `internal` metrics -
  consider a dedicated non-5xx variant that isn't alerted on.
- `error_type()` collapses 401/402/429 into `invalid_request` because the public
  taxonomy only has three `type`s. Fine per `CLAUDE.md`, but note it's coarse.
- Acceptance criterion "boot < 100 ms" is verified manually (M1); fold a real
  timing assertion into the M7 criterion benchmarks rather than a flaky unit test.
- Graceful shutdown is unit-tested via an injected shutdown future; the real
  SIGINT/SIGTERM path (`shutdown_signal`) has no integration test (hard to do
  portably). Acceptable; revisit if signal handling grows.

## Noted while building M2

- Embedding output is always a float array in v1. Base64 embeddings are decoded
  on the way IN (a client requesting `encoding_format: "base64"` won't error),
  but we do not re-encode on the way OUT. Add base64 *output* if a client needs it.
- Ollama drops the OpenAI-only `dimensions` field with a `debug!` log; a client
  asking for a specific dimension silently gets full-width vectors. Consider a
  400 (LM-1001) when an unsupported-but-meaningful field is set under a strict mode.
- `LM-1002` (payload too large, 413) is emitted by `RequestBodyLimitLayer` as a
  raw 413 without our JSON error envelope. Map the tower-http rejection to
  `GatewayError::PayloadTooLarge` for a consistent body.
- Cancellation tests use real (short) wall-clock delays rather than
  `tokio(start_paused)`; robust today but revisit if they flake under CI load.
  The HTTP-level disconnect test asserts the server stays responsive and the
  upstream got the request - the actual upstream abort is proven at the provider
  layer (conformance `scenario_cancellation_aborts_upstream`).

## Noted while building M3

- Cohere v2 embed requires an `input_type`; the gateway can't know query-vs-
  document intent, so it always sends `search_document`. Expose a per-request
  or per-model override (`input_type`) if a caller needs `search_query`.
- `usage.search_units` is only meaningful for Cohere; Jina and Voyage bill
  rerank in tokens, so they report `0`. If token-based rerank usage matters for
  M5 cost counting, widen `RerankUsage` (e.g. add `total_tokens`) rather than
  overloading `search_units`.
- Rerank `documents` accept string or `{text}` only. Cohere also allows
  arbitrary objects with a `rank_fields` selector - out of scope; reduce to text
  at the edge if a provider needs it.
- TEI serves one model per process and ignores the request `model`/`top_n`; the
  gateway truncates to `top_n` after sorting. The configured `upstream_id` is
  informational for TEI. A future health/introspection hook could verify the
  configured model matches what the TEI process actually serves.
- The four hosted rerank providers default `max_batch_size` conservatively
  (Cohere 96, Jina/Voyage/OpenAI-style large, TEI 32). Revisit against real
  provider limits; embeddings batching already exercises these.

## Noted while building M4 (slice 1 - non-streaming chat)

- **Streaming disconnect test is a no-hang assertion, not an abort assertion.**
  `streaming_client_disconnect_does_not_hang_server` proves the server stays
  responsive but not that the upstream connection was actually closed (M4
  acceptance criterion 2: "upstream closed in < 100 ms"). And because the interim
  single-shot `chat_stream` awaits the full `chat()` before the guard is moved
  into the SSE body, the moved-guard path is not exercised. Strengthen in the
  streaming slice: assert via wiremock that the upstream request was aborted,
  and add a case where the client cuts *after* the body starts.
- Anthropic `translate_request` copies message roles verbatim and does not
  normalise user/assistant alternation or drop/merge `tool`/empty-content
  messages (spec 4.3 bullet). Fine for the interim text path; complete with
  tool translation in the streaming slice.
- Anthropic responses set `created: 0` (the API returns no timestamp). Some
  OpenAI clients expect a real epoch; set `SystemTime::now()` if one complains.
- `to_sse_body`'s `Chain` drops the mapping closure (and thus the cancel guard)
  as soon as the chunk stream is exhausted, a hair before the `[DONE]` frame.
  Benign today (nothing left to cancel once the upstream is done), but the real
  streaming slice must not tie a live resource to guard survival through `[DONE]`.
- Interim single-shot emits `[DONE]` even after a mid-stream error frame.
  Harmless with one item; real streaming must terminate after an error.

## Noted while building M4 (slice 2 - zero-copy streaming)

- ~~**Acceptance criterion 5 (LM-3010) not yet implemented.**~~ **Resolved**
  (commit `076b909`, slice 3). When the upstream closes without a `[DONE]`
  terminator and without a transport error, the gateway now appends a terminal
  `data: {"error": {"code": "LM-3010"...}}` frame then closes cleanly. The
  lightweight tail-watcher (`EventStreamState::scan_frame` in
  `crates/server/src/chat.rs`) inspects only frame boundaries - it matches a
  line-anchored `\ndata: [DONE]` marker and keeps at most `DONE_MARKER.len() - 1`
  trailing bytes, so a terminator split across two frames is still detected and
  model content that merely contains the text can never spoof it. Covered by the
  unit guard tests in that module and the end-to-end wiremock test
  `upstream_stream_without_done_yields_fg3010_error_frame` (passthrough), plus
  the mid-stream error-frame tests (`mid_stream_provider_error_becomes_terminal_error_frame`
  and `resilience.rs`).

- **Tools on Gemini**: RESOLVED (issue #4). `translate_request` (google) now
  maps OpenAI `tools` to Gemini `tools[].functionDeclarations` and `tool_choice`
  to `toolConfig.functionCallingConfig`; assistant `tool_calls` become
  `functionCall` parts, role `tool` messages become `functionResponse` parts,
  and both the non-streaming and streaming translators surface Gemini
  `functionCall` parts as OpenAI `tool_calls`. Chose the full mapping over the
  LM-2002 rejection. Covered by unit tests and wiremock round-trips in
  `crates/server/tests/chat.rs`.

## Noted while building M5

- **Accurate per-model tokenizer deferred.** ADR 003's opt-in "accurate
  tokenizer via `spawn_blocking`" is not implemented: the fallback is the
  byte-heuristic only (O(bytes), inline, hot-path-safe). Adding `tokenizers`/
  tiktoken is a heavy dependency for marginal v1 benefit; the config knob and
  the `spawn_blocking` plumbing should land together when a user needs
  billing-grade estimates. Until then `estimated=true` counts are heuristic.
- **Streaming output estimation = data-frame count.** When a stream carries no
  usage (rare: `include_usage` is auto-requested and translators always emit
  usage), the output-token estimate is the number of `data:` frames (~1 token
  per delta for OpenAI-style streams). Crude but honest (`estimated=true`).
- **usage_log records successful requests only.** Refusals (401/402/429) and
  upstream failures are visible in logs/metrics but produce no usage row (no
  spend happened). If per-key rejection analytics matter, add a `status`-only
  row path - the column already exists.
- **Rejected requests still count toward RPM/TPM.** Quota bumps are not
  unwound when a later admission step (budget) refuses the request. Standard
  rate-limiter behaviour; documented here for the principle of least surprise.
- **DB-stored provider keys are boot-time only.** `PUT /admin/provider-keys`
  takes effect at the next restart; wire it into the M7 hot-reload path.
- **Metadata values are stringified** in `usage_log.metadata` (JSON object of
  strings). If typed filtering (numeric ranges) matters, store the original
  JSON value instead.
- **TPM debits the pre-call estimate, never adjusted.** Unlike the budget
  (reserved then settled to real usage), the tokens-per-minute window keeps the
  estimate (`max_tokens` or the 2048 default when absent). Conservative - can
  throttle early, can never overrun. Adjust post-call if it starves real users.
- **No zeroization of key material.** `MasterKey` and the raw env string are
  not zeroized on drop; the `zeroize` crate would close the residual-memory
  window. Low risk (single long-lived process), noted from the M5 review.

## M6 (resilience) - deferred

- **Per-provider connect timeout.** `connect` is a `reqwest::Client` setting and
  the gateway shares one pooled client across providers, so the connect timeout
  is process-wide. Per-provider connect would need one client per provider
  (losing cross-provider pooling); `first_token` and `total` are already
  per-provider. See ADR 005.
- **First-frame-peek streaming retry.** Streaming retry/fallback happens only at
  the *open* phase (send + status). A stream that opens 200 then errors on its
  very first frame is treated as committed (clean SSE error frame, no retry).
  Peeking the first frame before committing would let that case retry too -
  more code, marginal benefit; deferred.
- **Circuit-breaker map is unbounded by design.** One entry per (provider,
  model) actually seen; bounded by the configured surface, never by client
  input. If a future dynamic-model feature lets clients mint arbitrary model
  ids, add an LRU cap.
- **Health probe is a bare GET to `base_url`.** It proves reachability, not that
  the model endpoint works (no auth, no real inference). A per-kind lightweight
  liveness call (e.g. `GET /v1/models`) would be truer; deferred to keep the
  probe provider-agnostic and free.

## M7 (release) - deferred

- **Hot reload swaps the routing table only.** SIGHUP / file-watch re-validate
  the config and atomically swap the provider registry (ArcSwap). Server bind
  address, `[auth]`, `[resilience]` and pricing are read once at boot; changing
  them still needs a restart. Extending the swap to those is a follow-up.
- **Hot reload re-resolves env keys fresh; DB keys are a boot snapshot.** A
  reload re-reads provider keys from the environment and re-applies the DB-key
  snapshot captured at boot (so a reload never strips a stored key). *Rotating*
  a DB-stored key (`PUT /admin/provider-keys`) after boot still needs a restart
  to take effect - the snapshot is boot-time.
- **Anthropic/Gemini translation fuzzing** goes only as deep as the shared SSE
  parser today. Fuzzing the `translate_request`/`translate_response`/stream
  translators directly needs a small public (or `#[cfg(fuzzing)]`) shim over the
  currently-private functions.
- **Loaded throughput vs LiteLLM not measured in-repo.** The in-process overhead
  (~3 µs) is benchmarked; the full p50/p99/RAM/req·s head-to-head is a
  reproducible `bench/` harness (docker-compose + k6) run by the operator, not
  captured as a committed baseline.

## Backlog debt paid down (post-v0.1.0)

- **Full-config hot reload (DEBT-1)** - done. Reload now swaps pricing and the
  resilience policy (retry/timeouts/fallbacks) as well as the routing table,
  preserving circuit-breaker state. (Auth knobs + server bind still boot-time.)
- **Key-material zeroization (DEBT-2)** - done. `MasterKey` wipes on drop and
  the raw `LUMEN_MASTER_KEY` string is zeroized after use.
- **Richer health probe (DEBT-3)** - done for TEI (`/health` liveness; non-2xx =
  down). Other kinds keep bare host-reachability (no reliable unauthenticated
  liveness endpoint); a per-kind probe for vendor APIs remains out of scope.

## Noted while building M8 (vision - image input to chat)

- **Per-image token heuristic for the estimation fallback - done** (issue #9).
  The estimation fallback (upstream reports no `usage`) now adds a flat
  per-image constant (`85` tokens for `"detail": "low"`, `765` for
  `"high"`/`"auto"`/unset) instead of counting an image part as `0`. A true
  per-dimension tile count (OpenAI's `85 + 170 * tiles`) still needs decoded
  pixel dimensions, which the gateway does not extract from a `data:` URI
  today and remains out of scope (no image-byte inspection on the request
  path). See the
  [ADR 003 addendum](adr/003-token-accounting.md#addendum-m8--vision--image-input).
- **Anthropic/Gemini file/GCS image URIs.** Only inline base64 (`data:` URIs)
  and, where the provider fetches it itself (Anthropic), remote `http(s)` URLs
  are supported. Anthropic's `source: {type: "file", file_id: ...}` and
  Gemini's GCS `fileUri` sources are not modelled; add if a caller needs
  pre-uploaded-file references instead of inline bytes.

## Provider coverage - next candidates (post-rename)

- **Tier-2 clouds need dedicated kinds** (different auth/schema, not
  OpenAI-compatible): Azure OpenAI (deployment routing + api-version), AWS
  Bedrock (SigV4, per-model schemas), Google Vertex AI (GCP OAuth, regional
  endpoints). Each is a `provider-integrator` task with wiremock tests.
- **Cohere chat** (Command R/R+) - we ship Cohere embed+rerank; chat is a
  distinct schema.
- **Cloudflare Workers AI rerank** - the OpenAI-compatible `cloudflare` kind
  covers chat+embed; bge-reranker uses the native `/ai/run/{model}` endpoint
  with a Cloudflare-specific response, so rerank needs custom code.
- **More rerankers**: Mixedbread (mxbai-rerank), Pinecone Rerank, NVIDIA NIM
  rerank, Together LlamaRank - cheap differentiation for a first-class rerank
  gateway.

## Noted while building M8 (vision / image input)

- **`LM-2004` pre-flight is primary-only.** A remote `http(s)` image URL is
  rejected up front (`LM-2004`, 400) only when the model's *primary* provider
  can't fetch it (Gemini). If the primary accepts URLs (OpenAI) but a Gemini
  model is a *fallback*, a fail-over to Gemini surfaces as `LM-3002` (502,
  translation error) rather than a client 4xx - safe (never fetched, no retry
  loop) but a soft break of the 4xx/5xx separation (rule 8). Options if it ever
  bites: scan the whole chain in the pre-flight (rejects some primary-servable
  requests), or add a dedicated client-input `ProviderError` mapped to a 4xx.
- **Per-image token heuristic for the estimation fallback - done** (issue #9,
  see the note above under "M8 (vision - image input to chat)"). The remaining
  gap is a true dimension-based tile count, which needs image decoding this
  gateway deliberately does not do on the request path.
- **Provider-native image URI forms.** Anthropic/Gemini file & GCS URI image
  sources (beyond inline base64 + remote URL) are not modelled.
