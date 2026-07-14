# M5 — Auth, virtual keys & hard budgets

## Objective
Virtual keys with HARD budgets enforced in the request path — the gap that neither LiteLLM nor OpenRouter fills well (agentic workloads that drain credits without stopping). And the DB stays OFF the critical path.

## Tasks

### 5.1 Storage
- [x] sqlx + SQLite, embedded migrations (`sqlx::migrate!`)
- [x] Tables: `virtual_keys(id, key_hash, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled)`, `usage_log(id, key_id, model, capability, tokens_in, tokens_out, search_units, cost, latency_ms, status, ts)` — NO prompt/response column
- [x] Virtual keys: `fg-` + 32 random bytes; store an argon2/blake3 hash, never the cleartext
- [x] Provider keys optionally in the DB, encrypted with AES-256-GCM (master key via env `LUMEN_MASTER_KEY`); the default mode remains env vars

### 5.2 Enforcement in the request path — WITHOUT touching the DB
- [x] Key state (remaining budget, RPM/TPM counters) loaded into memory at boot in a `DashMap`/`ArcSwap`
- [x] Budget/quota check = memory read + atomic CAS. Request refused BEFORE the upstream call: 402 LM-4001 (budget exhausted), 429 LM-4002 (RPM), 429 LM-4003 (TPM)
- [x] Budget debit: pre-call estimation (max_tokens) reserved atomically, adjusted post-call with the real usage — no possible race between concurrent requests
- [x] Persistence: periodic flush (default 10 s) of the in-memory counters → DB. Crash = loss of at most 10 s of counting, never an undetected budget overrun on the next request

### 5.3 Asynchronous usage logging
- [x] BOUNDED mpsc channel (default 10,000) → writer task that batches the INSERTs (default: every 2 s or 500 entries)
- [x] Channel full → drop the log + increment the Prometheus counter `usage_log_dropped_total`. The request path NEVER blocks on logging (LiteLLM lesson #12067)
- [x] Configurable retention: purge usage_log > N days (background task, default 30 d)

### 5.4 Token counting (central promise — see ADR 003)
- [x] **A token count for EVERY request, every capability**, never `0` by default: chat (in + out), embeddings (in), rerank (search_units if available + query+documents tokens)
- [x] Priority source: usage reported by the upstream (`estimated = false`); otherwise fall back to estimation (`estimated = true`)
- [x] Fallback: lightweight heuristic (byte/char) by default, precise per-model tokenizer optional (config) run via `spawn_blocking` — NEVER a heavy tokenizer on the request path (pillar 1) *(heuristic implemented; the opt-in precise tokenizer goes to the backlog — heavy dependency, see `docs/backlog.md` § M5 — the "never a heavy tokenizer inline" invariant holds by construction)*
- [x] TEI (no upstream usage) → estimated tokens, never a silent zero
- [x] Fixed-cardinality Prometheus counters: `lumen_tokens_total{capability,model,provider,direction,estimated}`, `lumen_rerank_search_units_total{model,provider}`, `tokens_estimated_total`
- [x] Counting NEVER blocks or fails a request; precise estimation happens off the hot path (in the async writer)

### 5.4b Cost counting (consumer of the tokens above)
- [x] Per-model price table in the config (`cost_per_1m_input`, `cost_per_1m_output`, `cost_per_1k_searches`)
- [x] Cost derived from the tokens counted in 5.4; embeddings: input tokens only; rerank: search units
- [x] Usage extracted from the last chunk in streaming; if absent, estimation and `estimated: true` flag in the log and the response

### 5.5 Minimal admin API
- [x] `POST/GET/PATCH /admin/keys` protected by the master key — create/list/disable keys, adjust budgets
- [x] The creation response is the ONLY time the cleartext key is visible

### 5.6 Request metadata (Cloudflare AI Gateway style) — see ADR 002
- [x] `x-lumen-metadata` header (+ alias `cf-aig-metadata`): FLAT JSON object `key → (string|number|bool)`, parsed once at the edge into the request extensions (zero alloc if absent)
- [x] Bounds: ≤ 16 keys, key ≤ 64 B, value ≤ 256 B, header ≤ 4 KiB
- [x] Log sink: the full metadata is attached to the structured-log fields AND stored in a `metadata` column of `usage_log` (Cloudflare-style filtering)
- [x] Prometheus sink: ONLY the config allowlist keys (`telemetry.metadata_labels`, default empty) become labels; the others stay logs-only (cardinality bounded by the operator, never by the client)
- [x] Robustness: absent/malformed/out-of-bounds metadata → dropped with `warn!` + counter `metadata_rejected_total`, the request NEVER fails
- [x] Security: metadata is opaque, never inspected; document that it is logged and must not contain secrets or prompt content

## Acceptance criteria

> Status: criteria 1–11 covered by automated tests (`crates/server/tests/auth.rs`
> at the HTTP level + `crates/auth/tests/*` at the unit level). Notes:
> * Criterion 3: the request path NEVER touches the DB by construction;
>   tested with a dead writer (stand-in for a locked DB) — 70 requests
>   pass at full speed, each dropped entry is counted.
> * Criterion 5: the DB half tested by a full dump (`debug_dump`); on the logs side,
>   the plaintext is never passed to `tracing` and `PlaintextKey`/`MasterKey`
>   have a redacted `Debug` (unit tested).
> * Criterion 12: moot as things stand — only the inline O(bytes) heuristic
>   exists; the precise tokenizer (the only latency risk) is in the backlog and
>   will need to carry this latency test when it arrives.
1. Race test: 50 concurrent requests on a key with budget for 10 → exactly the requests covered by the budget pass, zero overrun (assert on the final atomic counter).
2. Test: budget exhausted → 402 BEFORE any upstream call (wiremock: zero requests received).
3. Test: locked/slow DB (simulated) → the API requests keep passing, only the flush is delayed; request-path p99 latency unchanged.
4. Test: saturated log channel → requests not blocked, dropped counter incremented.
5. Test: the cleartext virtual key appears neither in the DB nor in the logs (grep over captured logs + DB dump).
6. Test: restart → budgets reloaded from the DB, an exhausted key stays exhausted.
7. Test: valid `x-lumen-metadata` → appears in the usage log; only the allowlist keys become Prometheus labels; a key outside the allowlist adds NO time series.
8. Test: malformed or > bounds metadata → request still succeeds, `metadata_rejected_total` incremented, nothing in the labels.
9. Test: embeddings via TEI (upstream without usage) → the log AND `lumen_tokens_total` report a count > 0 with `estimated="true"`; never zero.
10. Test: embeddings via OpenAI (upstream with usage) → count = upstream value, `estimated="false"`.
11. Test: each capability (chat/embed/rerank) increments `lumen_tokens_total` with the right `capability`/`direction`; rerank also increments `lumen_rerank_search_units_total`.
12. Test: request-path p99 latency unchanged when tokenizer-based estimation is enabled (estimation stays off the hot path).
