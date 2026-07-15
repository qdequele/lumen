# Docs restructure: guides, examples, capability sections - design

- Status: approved (brainstormed 2026-07-15)
- Owner: docs
- Location note: this spec lives in `specs/design/`, the new home this same
  design establishes for design documents (moved out of `docs/superpowers/`).

## Context

The repo DX review (2026-07-15) found the documentation accurate and the
onboarding solid, but flat: the mdBook is a thin reference (providers, errors,
perf, ADRs) with no guides, no examples, and no per-capability documentation.
The published book (https://qdequele.github.io/lumen/) is not linked from
anywhere in the repo. Capability documentation (chat, embeddings, reranking)
and operations documentation (analytics, budgets, resilience, deployment) live
scattered across README feature blurbs, `config.example.toml` comments,
`monitoring/README.md`, and error-doc narratives.

## Goals

1. Dedicated, top-level documentation parts for each capability: chat,
   embeddings, reranking.
2. A dedicated operations part with analytics/accounting management as its
   lead content.
3. Task-oriented guides and runnable examples.
4. The mdBook becomes the canonical documentation home; the README slims down
   and points to it.
5. Fold in the doc-level fixes from the DX review while touching these files
   anyway.

## Non-goals

- Code changes, except one CI step validating example configs. The pre-flight
  unset-API-key error, MSRV CI job, issue/PR templates, and CONTRIBUTING CI
  gate documentation are separate follow-ups.
- New factual claims. Every new page consolidates existing, code-verified
  material (the DX review's accuracy audit confirmed the sources).
- Client SDK guides (OpenAI Python/JS pointed at LUMEN). Deliberately kept
  out per brainstorm; examples are config + curl scenario directories.

## Decisions (from brainstorm Q&A)

| Question | Decision |
|---|---|
| Canonical doc home | The mdBook. README slims to pitch + quickstart + link. |
| Analytics section shape | One "Operations" part; analytics/accounting leads it. |
| Examples format | `examples/<scenario>/` dirs: `config.toml` + `README.md` + `run.sh`. Validated in CI by `lumen --check-config`. |
| Review fixes | Doc-level fixes folded in; code-level fixes excluded. |
| Information architecture | Capability-first (option A). |

## New book navigation (SUMMARY.md)

New pages marked with (new); existing pages keep their content.

```
Introduction                       (rewritten: orientation + links to all parts)

Getting started
  Installation                     (new) binary / Docker / source, --check-config
  Quickstart                       (new) expanded README walkthrough, 3 capabilities
  Configuration basics             (new) file anatomy, env overrides, reload pointer

Chat
  Chat completions                 (new) /v1/chat/completions, params, error cases
  Streaming                        (new) SSE format, heartbeats, stream guards
  Vision (image input)             (new) modalities, translation, LM-2003/2004/2008
  Tool calling                     (new) two-leg roundtrip, streamed deltas, coverage

Embeddings
  Embeddings                       (new) /v1/embeddings, input formats
  Batching                         (new) max_batch_size splitting, ordered reassembly
  Multimodal embeddings            (new) image parts, [image_fetch] guards, LM-2005/6/7

Reranking
  Reranking                        (new) /v1/rerank Cohere format, search units

Operations
  Token accounting & cost          (new) ADR 003 in user terms, estimated flag, prices
  Metrics & dashboards             (new) all lumen_* metrics, Prometheus, monitoring/ rig
  Usage log & multi-tenant         (new) x-lumen-metadata, label allowlist, queries
  Keys, quotas & budgets           (new) enabling auth, admin API incl. PUT provider-keys
  Resilience tuning                (new) retries, fallbacks, circuit breaker, timeouts
  Deployment                       (new) Docker/binary, TLS proxy, headers, SIGHUP

Examples
  Index                            (new) one entry per examples/ scenario

Reference
  Providers                        (existing, unchanged)
  Error codes                      (existing, unchanged)
  Performance baseline             (existing, unchanged)

Architecture decisions
  ADR 001-006                      (adds the previously missing 006)

Project
  Backlog                          (existing)
  Contributing                     (existing, de-duplicated label table)
```

Content rules:

- Capability pages link into the Reference providers matrix; they never
  duplicate per-provider tables.
- Every claim on a new page is sourced from existing verified material:
  README features, `config.example.toml` comments, `monitoring/README.md`,
  `docs/errors.md` narratives, ADRs, and the milestone specs.

## examples/ directory

```
examples/
  README.md                     what is here + how to run any example
  minimal-chat/                 one OpenAI model, chat + streaming curl
  rag-pipeline/                 embeddings + rerank together
  multi-provider-fallback/      OpenAI to Anthropic chain, circuit breaker demo
  self-hosted/                  Ollama + TEI, fully keyless, works offline
  multi-tenant-analytics/       metadata labels + budgets, pairs with monitoring/
```

Each scenario contains `config.toml` (runnable), `README.md` (what it shows,
expected output), and `run.sh` (curl requests). A CI step runs
`lumen --check-config --config examples/<each>/config.toml` so example configs
cannot rot silently. This is the only code-adjacent change.

## README changes

Keep: pitch, capabilities/API table, the 5-minute quickstart, a compressed
provider matrix, benchmarks summary, security summary. Each Features
subsection shrinks to 2-3 lines linking to its book page. Add a prominent
link to https://qdequele.github.io/lumen/ near the top.

## Folded DX fixes (doc level)

1. Add ADR 006 to `SUMMARY.md`.
2. Move `docs/superpowers/specs/*.md` to `specs/design/`; update the two
   ROADMAP references; remove the "Design & planning" section (the plan file
   and specs) from the public book.
3. Add local `mdbook serve` instructions to CONTRIBUTING.md.
4. Fix the stale nine-kind comment in `config.example.toml` (twenty kinds).
5. De-duplicate the label table in `docs/contributing.md` (link to
   CONTRIBUTING.md instead of restating a drifted copy).

## Validation

- `mdbook build` passes (docs.yml already gates this on deploy; run locally too).
- All internal links in the book resolve (mdbook fails the build on missing
  SUMMARY targets; spot-check inline cross-links).
- Every `examples/*/config.toml` passes `lumen --check-config` (new CI step).
- No em-dash anywhere in the new files (CI `no-em-dashes` job).
- `cargo test --workspace` stays green (the `shipped_example_config_is_valid`
  test and everything else are untouched).

## Risks

- Duplication drift between README and book quickstarts: mitigated by keeping
  the README version minimal and linking to the book for anything deeper.
- Moving `docs/superpowers/` breaks ROADMAP links if missed: the two
  references (ROADMAP.md M8/M9 spec lines) are updated in the same change.
