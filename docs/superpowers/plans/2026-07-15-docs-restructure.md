# Docs Restructure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the LUMEN mdBook into the canonical, capability-first documentation home (guides for chat/embeddings/reranking, an Operations section led by analytics), add runnable `examples/` scenario dirs validated in CI, slim the README, and fold in the doc-level DX fixes.

**Architecture:** Pure documentation restructure driven by `specs/design/2026-07-15-docs-restructure-design.md`. Each task adds one book section (its pages plus its `docs/SUMMARY.md` part) so `mdbook build` stays green at every commit. The only code-adjacent change is one CI job running `lumen --check-config` over `examples/*/config.toml`.

**Tech Stack:** mdBook (SUMMARY.md nav, parts via `#` headers), TOML configs, curl/bash scripts, GitHub Actions.

## Global Constraints

- NO em-dash (U+2014) anywhere, in any file. CI `no-em-dashes` rejects them. Use hyphens, commas, colons, parentheses.
- NO new factual claims. Every statement on a new page must come from a listed source (README.md, config.example.toml comments, docs/providers.md, docs/errors.md, monitoring/README.md, docs/adr/*, CHANGELOG.md) or be verified in the code before writing it.
- `mdbook build` must pass after every task (SUMMARY targets must exist).
- Capability pages LINK to the providers matrix (`providers.md`); never duplicate per-provider tables.
- Conventional commits, one per task, scoped `docs(...)`, `ci(...)`, or `chore(...)`. Never add a Co-Authored-By line.
- The published book URL is `https://qdequele.github.io/lumen/`.
- Docs tone: match the existing pages - terse, factual, error codes named inline, config keys in backticks.
- Run before each commit: `git diff --cached --name-only -z | xargs -0 grep -l $'\u2014' || echo CLEAN` and require `CLEAN`.

---

### Task 1: Tooling + relocate design specs out of the public book

**Files:**
- Move: `docs/superpowers/specs/2026-07-14-vision-image-input-design.md` -> `specs/design/2026-07-14-vision-image-input-design.md`
- Move: `docs/superpowers/specs/2026-07-14-multimodal-embeddings-design.md` -> `specs/design/2026-07-14-multimodal-embeddings-design.md`
- Modify: `ROADMAP.md` (two spec references), `docs/SUMMARY.md`

**Interfaces:**
- Produces: a SUMMARY.md with parts in order `[Introduction]` / `# Reference` / `# Architecture decisions` (now including ADR 006) / `# Project`, which Tasks 2-9 insert new parts into. All later tasks assume `mdbook` is installed.

- [ ] **Step 1: Ensure mdbook is installed**

Run: `command -v mdbook || cargo install mdbook`
Expected: `mdbook --version` prints a version.

- [ ] **Step 2: Move the two design specs**

```bash
mkdir -p specs/design
git mv docs/superpowers/specs/2026-07-14-vision-image-input-design.md specs/design/
git mv docs/superpowers/specs/2026-07-14-multimodal-embeddings-design.md specs/design/
```

Note: `docs/superpowers/plans/` stays (it is only removed from the book nav, next step). `specs/design/2026-07-15-docs-restructure-design.md` already lives there.

- [ ] **Step 3: Update SUMMARY.md**

Replace the whole `# Design & planning` section (last 4 lines of the file) with nothing, and add ADR 006. The file becomes exactly:

```markdown
# Summary

[Introduction](introduction.md)

# Reference

- [Providers](providers.md)
- [Error codes](errors.md)
- [Performance baseline](perf-baseline.md)

# Architecture decisions

- [001 - Crate & lib naming](adr/001-crate-and-lib-naming.md)
- [002 - Request metadata header](adr/002-request-metadata-header.md)
- [003 - Token accounting](adr/003-token-accounting.md)
- [004 - Streaming passthrough](adr/004-streaming-passthrough.md)
- [005 - Resilience execution](adr/005-resilience-execution.md)
- [006 - Client cancellation error code](adr/006-client-cancellation-error-code.md)

# Project

- [Backlog](backlog.md)
- [Contributing](contributing.md)
```

- [ ] **Step 4: Update ROADMAP.md spec references**

In `ROADMAP.md` replace (M8, around line 91):
`Spec: docs/superpowers/specs/2026-07-14-vision-image-input-design.md.` -> `Spec: specs/design/2026-07-14-vision-image-input-design.md.`
and (M9, around line 109):
`Spec: docs/superpowers/specs/2026-07-14-multimodal-embeddings-design.md` -> `Spec: specs/design/2026-07-14-multimodal-embeddings-design.md`

Then run `grep -rn "docs/superpowers/specs" --include="*.md" .` and fix any other stale reference the same way (the vision plan file under `docs/superpowers/plans/` may reference its spec).

- [ ] **Step 5: Build and verify**

Run: `mdbook build`
Expected: success, and `book/` contains no `superpowers/` nav entries. Run `ls book/adr/ | grep 006` -> the ADR 006 HTML exists.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "docs(book): index ADR 006, move design specs to specs/design"
```

---

### Task 2: Getting started section (3 pages)

**Files:**
- Create: `docs/getting-started/installation.md`, `docs/getting-started/quickstart.md`, `docs/getting-started/configuration.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: SUMMARY layout from Task 1.
- Produces: the `# Getting started` part; later pages cross-link `getting-started/configuration.md`.

- [ ] **Step 1: Insert the part into SUMMARY.md**

Directly after the `[Introduction](introduction.md)` line (and its blank line), insert:

```markdown
# Getting started

- [Installation](getting-started/installation.md)
- [Quickstart](getting-started/quickstart.md)
- [Configuration basics](getting-started/configuration.md)
```

- [ ] **Step 2: Write `installation.md`**

Sources: README.md lines 90-114 (run modes), `Dockerfile` header comment, `.github/workflows/release.yml` (binary artifacts), README lines 240-248 (`--check-config`). Structure:

- `# Installation` intro sentence: single static binary, three ways to get it.
- `## Docker` - `docker run -p 8080:8080 -v ./config.toml:/config.toml -e OPENAI_API_KEY=... ghcr.io/qdequele/lumen:latest`; note the image sets `LUMEN_SERVER__HOST=0.0.0.0`; multi-arch amd64+arm64.
- `## Prebuilt binary` - static musl binaries (x86_64 and aarch64 linux) attached to each GitHub release (`v*` tags).
- `## From source` - `cargo build --release -p server --bin lumen` (recent stable toolchain; MSRV 1.80 per Cargo.toml), binary at `target/release/lumen`; run with `lumen --config config.toml`.
- `## Validate a config without booting` - `lumen --check-config --config config.toml`: parses, validates semantics, builds the provider registry, exits 0/non-zero; binds no listener, opens no DB, contacts no provider; safe in CI. Copy the wording from README, do not re-derive.
- End with links: Quickstart (next page), `config.example.toml` on GitHub.

- [ ] **Step 3: Write `quickstart.md`**

Source: README "5-minute quickstart" (lines 53-157) - reuse its config block and the three curl examples verbatim, expanded with the responses' shape. Structure:

- Goal line: zero to chat + embed + rerank.
- `## 1. Minimal config` - the same 24-line TOML from the README (openai: gpt-4o chat + text-embedding-3-small embed; cohere: rerank-english).
- `## 2. Run` - both Docker and `cargo run -p server -- --config config.toml`; note auth is off by default (no Authorization header needed) and unset provider keys only fail when a request routes to that provider.
- `## 3. Chat` / `## 4. Embeddings` / `## 5. Rerank` - the three curl commands from the README; after each, one sentence on the response (chat: OpenAI envelope + `usage`; embeddings: `data[].embedding` + `usage`; rerank: results sorted by descending `relevance_score`, empty `documents` rejected with `LM-2010`).
- `## Next steps` - links to chat/completions.md, embeddings/embeddings.md, reranking/reranking.md, operations/token-accounting.md (these pages exist by Task 7; forward links are fine, mdbook only checks SUMMARY targets).

- [ ] **Step 4: Write `configuration.md`**

Sources: config.example.toml comments (authoritative), README Configuration section (lines 257-273). Structure:

- One TOML file + `LUMEN_*` env overrides with `__` nesting (`LUMEN_SERVER__PORT=9090`); top-level keys must precede any `[table]` header.
- Section tour, one short paragraph each: `log_format`, `[server]` (host/port/body_limit/first_token_timeout_ms/sse_heartbeat_ms), `[auth]` (off by default, link operations/keys-budgets.md), `[telemetry]` (link operations/usage-log.md), `[resilience]` (all defaults live in config.example.toml, link operations/resilience.md), `[[providers]]` / `[[providers.models]]` (id/upstream_id/capabilities/modalities/costs/fallbacks, link providers.md), `[image_fetch]` (link embeddings/multimodal.md).
- Rule: API keys are never written in config; `api_key_env` names the env var.
- Hot reload pointer: SIGHUP or file watch, validated before swap; details in operations/deployment.md.
- `--check-config` cross-link to installation.md.

- [ ] **Step 5: Build, scan, commit**

Run: `mdbook build` (expect success), em-dash scan per Global Constraints.

```bash
git add docs/SUMMARY.md docs/getting-started
git commit -m "docs(book): getting started section (installation, quickstart, configuration)"
```

---

### Task 3: Chat section (4 pages)

**Files:**
- Create: `docs/chat/completions.md`, `docs/chat/streaming.md`, `docs/chat/vision.md`, `docs/chat/tool-calling.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: `# Getting started` part from Task 2.
- Produces: the `# Chat` part.

- [ ] **Step 1: Insert into SUMMARY.md after the Getting started block**

```markdown
# Chat

- [Chat completions](chat/completions.md)
- [Streaming](chat/streaming.md)
- [Vision (image input)](chat/vision.md)
- [Tool calling](chat/tool-calling.md)
```

- [ ] **Step 2: Write `completions.md`**

Sources: README API table + quickstart curl; docs/errors.md LM-2001/LM-2002/LM-1001/LM-1002; README resilience section (`x-lumen-model-used`); fuzz/README.md (unknown-field `extra` passthrough). Verify the passthrough claim in `crates/core/src/chat.rs` (serde flatten) before stating it. Structure:

- `POST /v1/chat/completions`, OpenAI request/response format; the model id is YOUR configured id (link providers.md for aliasing).
- Full curl + a trimmed example response JSON (choices, finish_reason, usage).
- Unknown request fields are forwarded to the upstream untouched (the `extra` passthrough), so provider-specific params keep working.
- Routing errors: unknown model `LM-2001` (404), model lacks the chat capability `LM-2002` (400), malformed body `LM-1001` (400), body too large `LM-1002` (413). Link errors.md.
- Fallbacks: if the model has `fallbacks`, the serving model is reported in the `x-lumen-model-used` response header. Link operations/resilience.md.
- Providers that serve chat: link the matrix in providers.md (do not restate it).

- [ ] **Step 3: Write `streaming.md`**

Sources: README (stream flag, `data: [DONE]`), config.example.toml `[server]` comments (first_token_timeout_ms, sse_heartbeat_ms), docs/errors.md LM-3010/LM-3011 + LM-6001 narrative, ADR 004. Structure:

- Add `"stream": true`; response becomes `text/event-stream` with `data: {...}` frames and terminal `data: [DONE]`.
- Passthrough design: upstream bytes are forwarded verbatim when the schema matches, no per-chunk re-serialization (link ADR 004).
- Heartbeats: `: ping` comment after `sse_heartbeat_ms` idle keeps proxies from reaping silent streams.
- Guards: no first frame within `first_token_timeout_ms` -> `LM-3011` (504); upstream dies without `[DONE]` -> terminal SSE error frame `LM-3010` (502).
- Client disconnect: upstream aborted; accounting settles at 499 `LM-6001` (link ADR 006 and errors.md).
- Usage in streams: upstream usage from the last chunk when present, else local estimate flagged `estimated` (link operations/token-accounting.md).

- [ ] **Step 4: Write `vision.md`**

Sources: README vision paragraph (lines 47-51), docs/providers.md vision section, docs/errors.md LM-2003/LM-2004/LM-2008, CHANGELOG `[Unreleased]` provider-native file/GCS URIs (#12). Structure:

- Opt-in per model: `modalities = ["text", "image"]` (default `["text"]`), surfaced in `GET /v1/models`.
- OpenAI content-parts shape example (text part + `image_url` part, data: URI and https URL).
- Pre-flight: image to a text-only model -> `LM-2003` (400) before any upstream call, enforced across the whole fallback chain.
- Per-provider handling: OpenAI-family and `vllm` forward parts verbatim; `anthropic` and `google` translate; a remote URL to Gemini -> `LM-2004` (400), the gateway never fetches chat image URLs.
- Provider-native sources: `anthropic-file:<id>` and Gemini `gs://` / Files API URIs; mismatched provider -> `LM-2008` (400).
- Token accounting: upstream usage authoritative; the local fallback estimate includes the per-image heuristic (see CHANGELOG #9), always flagged `estimated`.

- [ ] **Step 5: Write `tool-calling.md`**

Sources: monitoring/README.md function-calling paragraph, CHANGELOG `[Unreleased]` Gemini tool calling (#4). Before writing, verify current provider coverage in `crates/providers/src/` (Anthropic translation of tool_use, Gemini functionDeclarations) so the coverage list is exact at write time. Structure:

- OpenAI `tools` / `tool_calls` format in, same format out, for every chat provider (translated for Anthropic and Gemini, passed through for OpenAI-family).
- The two-leg flow with a compact example: request with `tools`, response `finish_reason: "tool_calls"`, follow-up request appending the `tool` role message, grounded final answer.
- Streaming: `tool_calls` deltas stream in OpenAI format.
- Coverage note as verified in code.

- [ ] **Step 6: Build, scan, commit**

`mdbook build`, em-dash scan.

```bash
git add docs/SUMMARY.md docs/chat
git commit -m "docs(book): chat section (completions, streaming, vision, tool calling)"
```

---

### Task 4: Embeddings section (3 pages)

**Files:**
- Create: `docs/embeddings/embeddings.md`, `docs/embeddings/batching.md`, `docs/embeddings/multimodal.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: `# Chat` part from Task 3.
- Produces: the `# Embeddings` part.

- [ ] **Step 1: Insert into SUMMARY.md after the Chat block**

```markdown
# Embeddings

- [Embeddings](embeddings/embeddings.md)
- [Batching](embeddings/batching.md)
- [Multimodal embeddings](embeddings/multimodal.md)
```

- [ ] **Step 2: Write `embeddings.md`**

Sources: README quickstart curl, CHANGELOG entries for input-format gaps (issues #25 / PR #44: pre-tokenized rejection) and `encoding_format`; docs/errors.md LM-1001. Verify accepted input shapes in `crates/core/src/embed.rs` (`EmbedInput`) before writing the list. Structure:

- `POST /v1/embeddings`, OpenAI format; curl + trimmed response (`data[].embedding`, `usage.prompt_tokens`).
- Accepted `input` shapes as verified in core types: single string, array of strings, and (where the provider supports them) pre-tokenized token-id arrays; content-part arrays for multimodal models (link multimodal.md).
- Pre-tokenized token-id arrays sent to a text-only provider (Cohere, TEI, Ollama, Jina, Voyage, Mistral) are rejected pre-flight with `LM-1001`, naming the provider (from errors.md LM-1001 row).
- `strict` mode: unsupported-but-meaningful fields (e.g. `dimensions` on Ollama) rejected with `LM-1001` instead of silently dropped (from config.example.toml comment).
- Providers that serve embed: link providers.md matrix.

- [ ] **Step 3: Write `batching.md`**

Source: docs/providers.md batching rule + the two batch-limit tables (link, do not copy the tables). Structure:

- A request larger than the provider's batch limit is split into sub-batches, run with bounded concurrency, and reassembled in the original input order; invisible to the client.
- Where limits come from: built-in per kind; the exact numbers live in the providers matrix (link both tables' anchors).
- Interaction with usage: one response, one aggregated `usage` count (verify aggregation in `crates/providers/src/` batching code before phrasing).

- [ ] **Step 4: Write `multimodal.md`**

Sources: docs/providers.md multimodal bullet (M9), config.example.toml `[image_fetch]` block, docs/errors.md LM-2005/2006/2007, ROADMAP M9. Structure:

- Declare `modalities = ["text", "image"]` on an embed model; `input` items may be strings or content-part arrays, mixable in one batch.
- Images as `data:` URIs always work. Remote http(s) URLs require `[image_fetch] enabled = true`, otherwise `LM-2005` (400).
- The guarded fetch: non-configurable private-IP block, connection pinning (DNS-rebinding safe), scheme/host/prefix allowlists, streamed size cap, timeout, `image/*` only, redirect re-validation, per-request count cap. Guard rejection -> `LM-2006` (400, reason logged server-side only); remote fetch failure -> `LM-2007` (502).
- Config reference: the full commented `[image_fetch]` block copied from config.example.toml.
- Per-provider semantics: Cohere embed-v4 and Voyage embed one combined text+image vector per item; Jina embeds one modality per item (from providers.md).
- Media accounting: `lumen_media_total`, `lumen_media_bytes_total` and usage_log columns (link operations/metrics.md).

- [ ] **Step 5: Build, scan, commit**

`mdbook build`, em-dash scan.

```bash
git add docs/SUMMARY.md docs/embeddings
git commit -m "docs(book): embeddings section (endpoint, batching, multimodal)"
```

---

### Task 5: Reranking section (1 page)

**Files:**
- Create: `docs/reranking/reranking.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: `# Embeddings` part from Task 4.
- Produces: the `# Reranking` part.

- [ ] **Step 1: Insert into SUMMARY.md after the Embeddings block**

```markdown
# Reranking

- [Reranking](reranking/reranking.md)
```

- [ ] **Step 2: Write `reranking.md`**

Sources: README quickstart rerank curl + LM-2010 note, config.example.toml cohere block (`cost_per_1k_searches`, multi-hop fallback chain), providers.md matrix. Structure:

- `POST /v1/rerank`, Cohere format (`query`, `documents`, `top_n`); curl + trimmed response (results sorted by descending `relevance_score`, each with `index` into the input list).
- Empty `documents` -> `LM-2010` (400).
- Billing: rerank is metered in search units (1 unit is approximately one query over up to 100 documents); configure `cost_per_1k_searches` per model; counted on `lumen_rerank_search_units_total` (link operations/token-accounting.md).
- One model can serve embed AND rerank (Cohere `embed-v4.0` example from config.example.toml).
- Cross-vendor fallback: the cohere -> jina -> voyage chain example from config.example.toml; `x-lumen-model-used` names the server.
- Providers that serve rerank: link providers.md matrix.

- [ ] **Step 3: Build, scan, commit**

`mdbook build`, em-dash scan.

```bash
git add docs/SUMMARY.md docs/reranking
git commit -m "docs(book): reranking section"
```

---

### Task 6: Operations section, analytics half (3 pages)

**Files:**
- Create: `docs/operations/token-accounting.md`, `docs/operations/metrics.md`, `docs/operations/usage-log.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: `# Reranking` part from Task 5.
- Produces: the `# Operations` part with its first three pages; Task 7 appends three more entries to the same part.

- [ ] **Step 1: Insert into SUMMARY.md after the Reranking block**

```markdown
# Operations

- [Token accounting & cost](operations/token-accounting.md)
- [Metrics & dashboards](operations/metrics.md)
- [Usage log & multi-tenant metadata](operations/usage-log.md)
```

- [ ] **Step 2: Write `token-accounting.md`**

Sources: ADR 003 (authoritative), README observability section, ROADMAP cross-cutting promise. Structure:

- The promise: EVERY request of every capability produces a token count, never a silent zero. Upstream usage when reported; otherwise a local estimate flagged `"estimated": true`.
- Where it surfaces: response body `usage`, Prometheus counters, and (auth on) the `usage_log` table.
- How estimation works: byte heuristic, hot-path safe; streaming estimates when the upstream sends no usage; images use the per-image heuristic (chat) or count zero (embeddings), always flagged (from ADR 003 + CHANGELOG #9).
- Cost: `cost_per_1m_input` / `cost_per_1m_output` / `cost_per_1k_searches` per model feed cost accounting and hard budgets; a model without prices costs 0, so budgets never bite on it (config.example.toml comment).
- Link ADR 003, operations/keys-budgets.md, operations/metrics.md.

- [ ] **Step 3: Write `metrics.md`**

Sources: monitoring/README.md panel table (the fullest metric list) and README key-metrics list. Verify each metric name exists in `crates/telemetry/src/` before listing. Structure:

- `GET /metrics`, Prometheus exposition; unauthenticated by design, restrict at the network layer (SECURITY.md).
- A table of every `lumen_*` metric with labels and meaning: `lumen_tokens_total{capability,model,provider,direction,estimated}`, `lumen_tokens_estimated_total`, `lumen_rerank_search_units_total`, `lumen_media_total`, `lumen_media_bytes_total`, `lumen_http_request_duration_seconds{method,path,status}`, `lumen_request_duration_seconds{capability,model,provider,status}`, `lumen_circuit_state` (0 closed / 1 open / 2 half-open), `lumen_provider_up`, `lumen_usage_log_dropped_total`, `lumen_metadata_rejected_total`, `lumen_config_reloads_total`, `lumen_config_reload_failures_total`.
- The monitoring rig: one paragraph pointing at `monitoring/` (compose stack, provisioned Grafana dashboard, smoke + traffic scripts) as the fastest way to see all of this live.

- [ ] **Step 4: Write `usage-log.md`**

Sources: ADR 002, config.example.toml `[auth]` + `[telemetry]` comments, monitoring/README.md multi-tenant section, errors.md LM-6001 note on 499 settlement. Structure:

- The usage log (auth on): token counts, cost, status, model_used, metadata - never message content. Written via a bounded channel to a batched async writer; when full it drops and increments `lumen_usage_log_dropped_total`, never blocking the request path. Retention via `retention_days`.
- `x-lumen-metadata` (alias `cf-aig-metadata`): flat JSON object, limits (16 keys, 64 B keys, 256 B values, 4 KiB header), attached to logs and usage_log; never inspected.
- Prometheus label allowlist: only `telemetry.metadata_labels` keys become labels; empty default means clients can never mint a time series; keep value sets bounded (cardinality warning).
- Multi-tenant recipe from monitoring/README.md: send org/team/project ids in the header, allowlist them, query e.g. `sum by (org_id) (increase(lumen_tokens_total{org_id!=""}[24h]))`; full curl example copied over.
- Disconnect accounting: a mid-stream client disconnect settles usage_log.status and the duration sample at 499 instead of a fake 200.

- [ ] **Step 5: Build, scan, commit**

`mdbook build`, em-dash scan.

```bash
git add docs/SUMMARY.md docs/operations
git commit -m "docs(book): operations section part 1 (accounting, metrics, usage log)"
```

---

### Task 7: Operations section, management half (3 pages)

**Files:**
- Create: `docs/operations/keys-budgets.md`, `docs/operations/resilience.md`, `docs/operations/deployment.md`
- Modify: `docs/SUMMARY.md`

**Interfaces:**
- Consumes: the `# Operations` part from Task 6.
- Produces: completed Operations part. Examples (Task 8-9) link to resilience.md and keys-budgets.md.

- [ ] **Step 1: Append to the Operations block in SUMMARY.md**

After the usage-log line, add:

```markdown
- [Keys, quotas & budgets](operations/keys-budgets.md)
- [Resilience tuning](operations/resilience.md)
- [Deployment](operations/deployment.md)
```

- [ ] **Step 2: Write `keys-budgets.md`**

Sources: README auth section, SECURITY.md guarantees, config.example.toml `[auth]` comments, errors.md LM-4001..4004. FIRST read `crates/server/src/app.rs` lines 70-98 and the admin handlers to get the exact admin route list and request/response shapes; document what the code does, including `PUT /admin/provider-keys/{name}`. Structure:

- Enabling: `[auth] enabled = true` + `LUMEN_MASTER_KEY` (64 hex chars); off by default (open proxy, no DB at all).
- What you get: virtual keys (BLAKE3-hashed, plaintext shown once), hard budgets, RPM/TPM quotas - all enforced in memory before any upstream call; the DB is never on the request path; a crash loses at most `flush_interval_ms` of accounting, never allows an overrun.
- Refusals: 402 `LM-4001` budget, 429 `LM-4002` RPM, 429 `LM-4003` TPM, 401 `LM-4004` missing/invalid key (deliberately unspecific).
- The admin API: master-key gated; document each route exactly as found in app.rs with a curl example for create-key (budget + quotas) and the provider-keys PUT (AES-256-GCM at rest under the master key).
- Operator notes from SECURITY.md: protect the master key + SQLite file together.

- [ ] **Step 3: Write `resilience.md`**

Sources: config.example.toml `[resilience]` comments (authoritative for defaults), errors.md "How resilience shapes these codes", ADR 005, README resilience section. Structure:

- Overview: retries -> fallbacks -> circuit breaker, all off the DB, defaults always on; only per-model `fallbacks` and health checks are opt-in.
- Retries: retryable failures only (5xx, timeouts, 429), never a client 4xx; exponential backoff with equal jitter; honors Retry-After as a floor; streaming retries only before the first chunk reached the client. Keys: `retry_max_attempts`, `retry_base_ms`, `retry_max_ms`.
- Fallback chains: per-model `fallbacks`, validated at boot (must exist and serve every declared capability); `x-lumen-model-used` reports the server.
- Circuit breaker: per (provider, model); `circuit_failure_threshold` consecutive failures open it, `circuit_cooldown_ms` then one half-open probe; open circuit skips instantly to the next fallback or answers 503 `LM-3020` + Retry-After.
- The three timeouts and their codes: connect `LM-3012` (client-wide), first-token `LM-3011` (in `[server]`, per-provider overridable), total `LM-3013` (in `[resilience]`, per-provider overridable).
- Health checks: opt-in, background only, probes only providers with an explicit `base_url`; `GET /health/providers` + `lumen_provider_up`; `/health` itself never does I/O.
- The error-code mapping table from errors.md section "How resilience shapes these codes" (link rather than copy; one summarizing sentence).

- [ ] **Step 4: Write `deployment.md`**

Sources: Dockerfile, README (Docker run, hot reload, security headers), SECURITY.md operator responsibilities, release.yml. Structure:

- Docker: the run command; image is distroless/static, multi-arch, sets `LUMEN_SERVER__HOST=0.0.0.0`; mount the config at `/config.toml`.
- Bare binary: static musl release binaries; systemd-friendly single process; bind host/port via `[server]` or `LUMEN_SERVER__*`.
- TLS: intentionally not terminated by LUMEN; put a reverse proxy in front; HSTS left to the proxy; default security headers listed (nosniff, DENY, no-referrer, CSP default-src 'none').
- Surface control: restrict `/admin/*` and `/metrics` at the network layer; `/health` for liveness probes (no I/O).
- Hot reload: SIGHUP or file watch; new config validated then atomically swapped; in-flight requests unaffected; a bad config is rejected and `lumen_config_reload_failures_total` increments.
- Validate configs in the deploy pipeline with `lumen --check-config` (link getting-started/installation.md).

- [ ] **Step 5: Build, scan, commit**

`mdbook build`, em-dash scan.

```bash
git add docs/SUMMARY.md docs/operations
git commit -m "docs(book): operations section part 2 (keys and budgets, resilience, deployment)"
```

---

### Task 8: examples/ scenarios, part 1 (index + three scenarios)

**Files:**
- Create: `examples/README.md`, `examples/minimal-chat/{config.toml,README.md,run.sh}`, `examples/self-hosted/{config.toml,README.md,run.sh}`, `examples/multi-provider-fallback/{config.toml,README.md,run.sh}`

**Interfaces:**
- Produces: the `examples/<scenario>/` layout (config.toml + README.md + run.sh) that Task 9 extends and Task 10's CI job globs (`examples/*/config.toml`). run.sh contract: `BASE_URL` env (default `http://localhost:8080`), `set -euo pipefail`, assumes the gateway is already running with the scenario config.

- [ ] **Step 1: Write `examples/README.md`**

Content: what each scenario demonstrates (one line each, matching the six dirs after Task 9), and how to run any of them:

```bash
# terminal 1 - start the gateway with the scenario's config
cargo run -p server -- --config examples/minimal-chat/config.toml
# terminal 2 - fire the scenario's requests
./examples/minimal-chat/run.sh
```

Also: every config passes `lumen --check-config` in CI; keys are read from env vars named in each config.

- [ ] **Step 2: Write `minimal-chat`**

`config.toml`:

```toml
# Minimal chat: one provider, one model. Needs OPENAI_API_KEY.
[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
upstream_id = "gpt-4o-2024-08-06"
capabilities = ["chat"]
```

`run.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"

echo "== non-streaming chat =="
curl -sf "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Say hello in one word."}]}'
echo

echo "== streaming chat =="
curl -sfN "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"Count to three."}]}'
echo
```

`README.md`: what it shows (smallest possible config; streaming = same endpoint + `"stream": true`), the two-terminal run recipe, expected output shape (OpenAI envelope; SSE `data:` frames ending in `data: [DONE]`). `chmod +x run.sh`.

- [ ] **Step 3: Write `self-hosted`**

`config.toml` (fully keyless; chat via the `vllm` kind pointed at Ollama's OpenAI-compatible endpoint, embeddings via the `ollama` kind, rerank via `tei`):

```toml
# Fully self-hosted, no API keys: Ollama (chat via its OpenAI-compatible
# endpoint, embeddings natively) + TEI (rerank). Works offline.
[[providers]]
name = "ollama-openai"
kind = "vllm"                       # any OpenAI-compatible server
base_url = "http://localhost:11434/v1"
first_token_timeout_ms = 60000      # first call may load the model into VRAM
total_timeout_ms = 120000

[[providers.models]]
id = "llama"
upstream_id = "llama3.2"
capabilities = ["chat"]

[[providers]]
name = "ollama-native"
kind = "ollama"
base_url = "http://localhost:11434"

[[providers.models]]
id = "nomic-embed"
upstream_id = "nomic-embed-text"
capabilities = ["embed"]

[[providers]]
name = "tei"
kind = "tei"
base_url = "http://localhost:8081"

[[providers.models]]
id = "bge-reranker"
upstream_id = "BAAI/bge-reranker-large"
capabilities = ["rerank"]
```

`run.sh`: same header contract; three curls (chat to `llama`, embeddings to `nomic-embed`, rerank to `bge-reranker` with two documents). `README.md`: prerequisites (`ollama pull llama3.2`, `ollama pull nomic-embed-text`, a TEI container command for the reranker with its published port mapped to 8081), the run recipe, and a note that rerank is skippable if TEI is not running.

- [ ] **Step 4: Write `multi-provider-fallback`**

`config.toml` (from config.example.toml's pattern):

```toml
# Cross-vendor chat fallback: OpenAI primary, Anthropic fallback.
# Needs OPENAI_API_KEY and ANTHROPIC_API_KEY (fallback only fires when used).
[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
upstream_id = "gpt-4o-2024-08-06"
capabilities = ["chat"]
fallbacks = ["claude-3-5-sonnet"]

[[providers]]
name = "anthropic"
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[[providers.models]]
id = "claude-3-5-sonnet"
upstream_id = "claude-3-5-sonnet-20241022"
capabilities = ["chat"]
```

`run.sh`: one chat request printing the `x-lumen-model-used` response header (`curl -sf -D - ... | grep -i x-lumen-model-used` then the body). `README.md`: how to see the fallback fire (set `OPENAI_API_KEY` to a bogus value, send 5+ requests to trip the circuit breaker at `circuit_failure_threshold = 5` default, watch `x-lumen-model-used` flip to `claude-3-5-sonnet`); link the book's operations/resilience.md.

- [ ] **Step 5: Validate configs and commit**

```bash
chmod +x examples/*/run.sh
for cfg in examples/*/config.toml; do cargo run -q -p server -- --check-config --config "$cfg"; done
```

Expected: `config OK` for all three. Em-dash scan. Then:

```bash
git add examples
git commit -m "docs(examples): minimal-chat, self-hosted, multi-provider-fallback scenarios"
```

---

### Task 9: examples/ scenarios, part 2 + book Examples page

**Files:**
- Create: `examples/rag-pipeline/{config.toml,README.md,run.sh}`, `examples/multi-tenant-analytics/{config.toml,README.md,run.sh}`, `docs/examples.md`
- Modify: `docs/SUMMARY.md`, `examples/README.md` (add the two lines if not present)

**Interfaces:**
- Consumes: run.sh contract from Task 8; Operations pages from Tasks 6-7 for links.
- Produces: the `# Examples` book part.

- [ ] **Step 1: Write `rag-pipeline`**

`config.toml`:

```toml
# RAG retrieval pair: embeddings (OpenAI) + reranking (Cohere).
# Needs OPENAI_API_KEY and COHERE_API_KEY.
[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "text-embedding-3-small"
capabilities = ["embed"]

[[providers]]
name = "cohere"
kind = "cohere"
api_key_env = "COHERE_API_KEY"

[[providers.models]]
id = "rerank-english"
upstream_id = "rerank-v3.5"
capabilities = ["rerank"]
cost_per_1k_searches = 2.0
```

`run.sh`: embed 3 document strings, then rerank the same 3 documents against a query with `top_n: 2`; comments explain the RAG shape (embed at index time, rerank at query time). `README.md`: the pipeline story plus links to the book's embeddings and reranking sections.

- [ ] **Step 2: Write `multi-tenant-analytics`**

`config.toml`:

```toml
# Multi-tenant accounting: metadata labels on Prometheus + virtual keys with
# a hard budget. Needs OPENAI_API_KEY and LUMEN_MASTER_KEY (64 hex chars).
[telemetry]
metadata_labels = ["org_id", "team_id", "project_id"]

[auth]
enabled = true
db_path = "examples-multi-tenant.db"

[[providers]]
name = "openai"
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
upstream_id = "gpt-4o-2024-08-06"
capabilities = ["chat"]
cost_per_1m_input = 2.5
cost_per_1m_output = 10.0
```

Before writing `run.sh`, read `crates/server/src/app.rs` (admin routes) and the admin handler request shapes; then `run.sh` does: create a virtual key via the admin API (master key bearer) with a small budget, send 2 chat requests with that key and an `x-lumen-metadata` header (`{"org_id":"acme","team_id":"search","project_id":"docs"}`), then `curl -sf $BASE_URL/metrics | grep 'lumen_tokens_total.*org_id="acme"'`. `README.md`: generate a master key with `openssl rand -hex 32`, export it, boot recipe, what to look for on /metrics, link operations/usage-log.md and operations/keys-budgets.md. Note the `.db` file is created next to the CWD and is gitignored (add `examples-multi-tenant.db` to `.gitignore`).

- [ ] **Step 3: Verify check-config on the auth-enabled config**

Run: `cargo run -q -p server -- --check-config --config examples/multi-tenant-analytics/config.toml`
If it demands `LUMEN_MASTER_KEY`, re-run with `LUMEN_MASTER_KEY=$(printf '0%.0s' $(seq 64))` and record in the scenario README + Task 10's CI job that the env var is required for validation.

- [ ] **Step 4: Write `docs/examples.md` and index it**

Page: one section per scenario (all six lines mirrored from `examples/README.md`), each with: what it demonstrates, required env vars, the two-terminal run recipe, and a link to the directory on GitHub (`https://github.com/qdequele/lumen/tree/main/examples/<name>`). Add to SUMMARY.md between the Operations block and `# Reference`:

```markdown
# Examples

- [Examples](examples.md)
```

- [ ] **Step 5: Validate, build, commit**

```bash
for cfg in examples/*/config.toml; do LUMEN_MASTER_KEY=$(printf '0%.0s' $(seq 64)) cargo run -q -p server -- --check-config --config "$cfg"; done
mdbook build
```

Em-dash scan. Then:

```bash
git add examples docs/SUMMARY.md docs/examples.md .gitignore
git commit -m "docs(examples): rag-pipeline and multi-tenant-analytics scenarios, book index"
```

---

### Task 10: CI job validating example configs

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `examples/*/config.toml` glob from Tasks 8-9.

- [ ] **Step 1: Append the job to ci.yml**

At the end of the `jobs:` map (after the `supply-chain` job), add:

```yaml
  examples:
    name: example configs
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4

      - name: Install stable toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo artifacts
        uses: Swatinem/rust-cache@v2

      - name: Validate every example config
        env:
          # check-config only; any 64-hex value satisfies the auth example
          LUMEN_MASTER_KEY: "0000000000000000000000000000000000000000000000000000000000000000"
        run: |
          cargo build -q -p server --bin lumen
          for cfg in examples/*/config.toml; do
            echo "== $cfg"
            ./target/debug/lumen --check-config --config "$cfg"
          done
```

Match the indentation and trigger style of the existing jobs (the workflow's existing `on:` block already covers push + pull_request; do not change it).

- [ ] **Step 2: Verify locally and lint**

Run the loop from the yaml block locally (expect all `config OK`). If `actionlint` is installed, run it on ci.yml; otherwise validate the YAML parses: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: validate examples/*/config.toml with lumen --check-config"
```

---

### Task 11: Introduction rewrite + README slimming

**Files:**
- Modify: `docs/introduction.md`, `README.md`

**Interfaces:**
- Consumes: every book part from Tasks 2-9 (links must resolve).

- [ ] **Step 1: Rewrite `docs/introduction.md`**

Keep the pitch (first two paragraphs) and the four pillars verbatim. Replace the "What's here" list with a map of the new parts: Getting started (install, quickstart, config), Chat / Embeddings / Reranking (one line each), Operations (accounting, metrics, multi-tenant, keys and budgets, resilience, deployment), Examples, Reference (providers, errors, perf), ADRs, Project. Remove the sentence deferring the quickstart to the GitHub README (the book now owns it); keep the contributing link.

- [ ] **Step 2: Slim README.md**

Precise edits, keeping everything else:

1. After the intro paragraphs (below line 19), add: `**Full documentation: <https://qdequele.github.io/lumen/>** - guides per capability, operations (analytics, budgets, resilience), examples, and reference.`
2. Keep unchanged: Contents, Capabilities & API table, the whole 5-minute quickstart, the two provider tables, Benchmarks, Security, License.
3. Replace each `###` subsection body under `## Features` (Auth/Resilience/Observability/Hot reload/--check-config/Security headers) with 2-3 sentences: keep the first sentence or two of the existing text (they are accurate), delete the deep detail (metric lists, code lists, header lists), and end each with a link to its book page: `operations/keys-budgets`, `operations/resilience`, `operations/token-accounting` + `operations/metrics`, `operations/deployment` (hot reload), `getting-started/installation` (--check-config), `operations/deployment` (headers). Use absolute book URLs (`https://qdequele.github.io/lumen/operations/keys-budgets.html` style; verify the exact .html paths in the built `book/` dir).
4. In `## Configuration`, replace the section-by-section bullet list with two sentences: everything is one TOML file plus `LUMEN_*` env overrides; the commented reference is config.example.toml and the walkthrough lives in the book (`getting-started/configuration.html`).
5. Add to `## Reference` list: `- Examples: [examples/](examples/)` and `- Documentation site: <https://qdequele.github.io/lumen/>`.
6. Add one line after the quickstart intro sentence pointing at `examples/` for ready-made scenario configs.

- [ ] **Step 3: Build, verify links, commit**

`mdbook build`; then check every book URL used in README exists in `book/`: for each `.html` path referenced, `test -f book/<path>`. Em-dash scan.

```bash
git add docs/introduction.md README.md
git commit -m "docs: book-first introduction, slim README feature blurbs to links"
```

---

### Task 12: Remaining folded fixes + final validation

**Files:**
- Modify: `CONTRIBUTING.md`, `config.example.toml`, `docs/contributing.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: everything prior; this is the closing sweep.

- [ ] **Step 1: CONTRIBUTING.md - local docs build**

After the "Development setup" code block, add:

```markdown
### Documentation site

The docs in `docs/` build into an mdBook published at
<https://qdequele.github.io/lumen/> (see `.github/workflows/docs.yml`).
To work on them locally:

```bash
cargo install mdbook
mdbook serve --open     # live-reloading preview from the repo root
```

`docs/SUMMARY.md` is the navigation; a page not listed there does not appear.
```

(Nested fences: use four backticks for the outer block when writing the file, or restructure; verify rendering.)

- [ ] **Step 2: config.example.toml - fix the kind list comment**

Replace the comment lines (around lines 126-128):

```
# Each `[[providers]]` block declares one upstream. `kind` selects the built-in
# implementation: openai | anthropic | cohere | ollama | tei | jina | voyage |
# mistral | google. `name` is your own label and must be unique.
```

with:

```
# Each `[[providers]]` block declares one upstream. `kind` selects the built-in
# implementation. Native kinds: openai | anthropic | cohere | ollama | tei |
# jina | voyage | mistral | google. OpenAI-compatible kinds (chat + embed via
# the OpenAI path, built-in base URL): groq | together | fireworks | deepseek |
# openrouter | perplexity | xai | deepinfra | huggingface | cloudflare | vllm.
# See docs/providers.md for the full matrix. `name` is your own label and must
# be unique.
```

Then run `cargo test -p server shipped_example_config_is_valid` (comment-only change; must still pass).

- [ ] **Step 3: docs/contributing.md - de-duplicate the label table**

Replace the `## The label axes at a glance` section (the whole table) with one sentence: `Issues are classified along four axes (Type, priority:, area:, scope:); the canonical taxonomy with label meanings is in [CONTRIBUTING.md](https://github.com/qdequele/lumen/blob/main/CONTRIBUTING.md#issue--pr-labels).`

- [ ] **Step 4: CHANGELOG entry**

Under `## [Unreleased]` add a `### Changed` (or extend it) entry:

```markdown
- Documentation restructured around capabilities: the mdBook at
  https://qdequele.github.io/lumen/ is now the canonical documentation home
  (getting started, chat / embeddings / reranking guides, operations incl.
  analytics and budgets, examples); the README slimmed down accordingly.
  Added runnable `examples/` scenarios validated in CI by `--check-config`.
```

- [ ] **Step 5: Final validation sweep**

```bash
mdbook build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
git grep -n $'\u2014' -- '*.md' '*.toml' '*.yml' ':!Cargo.lock' ':!LICENSE' || echo CLEAN
for cfg in examples/*/config.toml; do LUMEN_MASTER_KEY=$(printf '0%.0s' $(seq 64)) ./target/debug/lumen --check-config --config "$cfg"; done
```

Expected: all green, `CLEAN`, all `config OK`.

- [ ] **Step 6: Commit**

```bash
git add CONTRIBUTING.md config.example.toml docs/contributing.md CHANGELOG.md
git commit -m "docs: local mdbook instructions, full kind list, de-duplicated labels, changelog"
```
