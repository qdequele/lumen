# Contributing to LUMEN

Thanks for your interest in LUMEN - a lightweight, fast, self-hostable gateway
for **chat/LLM, embeddings and reranking**. This guide covers how to set up,
what the bar is, and how we label and land work.

Before writing code, skim the [README](README.md) for what LUMEN is and the
[`CLAUDE.md`](CLAUDE.md) for the project's non-negotiable pillars and strict
code rules - they are the contract, not suggestions.

## Ground rules

The five pillars, in priority order, decide every trade-off:

1. **Performance** - < 1 ms added latency p99, zero-copy streaming, ~15 MB RAM idle.
2. **Sovereignty** - zero telemetry, prompts never logged by default, single binary.
3. **Robustness** - propagated cancellation, backpressure, DB off the request path.
4. **Multi-capability** - chat + embeddings + rerank are first-class citizens.
5. **Token observability** - every request of every capability produces a token
   count, never a silent zero (ADR 003).

Hard code rules (full list in [`CLAUDE.md`](CLAUDE.md#strict-code-rules)):

- No `unwrap()` / `expect()` / `panic!()` outside tests and `main.rs`.
- Never block the tokio runtime (no sync I/O, no `std::thread::sleep`).
- Every provider call takes a `CancellationToken`.
- No synchronous DB write in the request path - logging goes through a bounded
  channel to an async batched writer.
- Provider secrets are never logged, never in errors, never in `Debug`.
- `thiserror` in libraries, `anyhow` only in `main.rs`.
- Every error has a stable code (`LM-1001`, …) documented in [`docs/errors.md`](docs/errors.md).

## Development setup

```bash
# rust-toolchain.toml selects the floating `stable` channel (rustup honors
# it); the supported floor is MSRV 1.94, declared in Cargo.toml and checked
# in CI.
rustup update

cargo build --workspace
cargo run -p server -- --config config.example.toml   # run locally
```

### Documentation site

The docs in `docs/` build into an mdBook published at
<https://qdequele.github.io/lumen/> (see `.github/workflows/docs.yml`).
To work on them locally:

```bash
cargo install mdbook
mdbook serve --open     # live-reloading preview from the repo root
```

`docs/SUMMARY.md` is the navigation; a page not listed there does not appear.

## Validation - must pass before you open a PR

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # clippy pedantic
cargo fmt --check
```

Definition of Done for any change touching source:

- [ ] Unit tests + at least one integration test (wiremock for providers)
- [ ] No clippy warning
- [ ] Cancellation tested if the change touches the request path
- [ ] A test proving no secret leaks into logs, if secrets are involved
- [ ] Doc comments on any new public API

## Writing style

- **No em-dashes.** The U+2014 character is not allowed anywhere in the repo
  (source, docs, config, commit messages). Use a hyphen, comma, colon,
  parentheses, or restructure the sentence. CI enforces this with a
  `no-em-dashes` check, so a stray em-dash fails the build.

## Commits & pull requests

- **Conventional commits**, scoped and atomic - one logical change per commit:
  `feat(router): fallback chain with circuit breaker`,
  `fix(providers): reject remote image URLs for Gemini`,
  `docs(errors): document LM-3010`.
- Keep the PR focused. Note any user-visible change in [`CHANGELOG.md`](CHANGELOG.md).
- If you make an architecture choice not covered by the specs, write a short ADR
  in [`docs/adr/NNN-title.md`](docs/adr/) **before** implementing.
- Adding a provider? Follow the repeatable pattern - capability traits, schema
  translation, streaming, cancellation, and wiremock tests. See
  [`docs/providers.md`](docs/providers.md).

## Issue & PR labels

Issues are classified along four independent axes. An issue usually carries one
label from each relevant axis; cross-cutting infrastructure may have no `scope:`.

### Type - what kind of work

| Label | Meaning |
|-------|---------|
| `bug` | Something isn't working. |
| `enhancement` | New feature or request. |
| `documentation` | Docs improvements or additions. |
| `question` | Further information is requested. |
| `good first issue` | Good for newcomers. |
| `help wanted` | Extra attention is needed. |
| `duplicate` / `invalid` / `wontfix` | Triage outcomes. |

### `priority:` - how urgent

| Label | Meaning |
|-------|---------|
| `priority: high` | Correctness/spec gap or high-demand feature. |
| `priority: medium` | Useful robustness or feature work. |
| `priority: low` | Nice-to-have / edge case. |

### `area:` - which subsystem

| Label | Meaning |
|-------|---------|
| `area: providers` | Provider integrations & translation. |
| `area: streaming` | SSE streaming / chat path. |
| `area: tokenizer` | Token counting & estimation (ADR 003). |
| `area: observability` | Metrics, `usage_log`, tracing. |
| `area: config` | Config, hot reload, ops surface. |
| `area: resilience` | Retries, timeouts, circuit breaker, health. |
| `area: testing` | Tests, fuzzing, benchmarks. |
| `area: vision` | Image input to chat. |

### `scope:` - which capability

The gateway's three first-class capabilities. Apply one or more; omit entirely
for cross-cutting work (config, resilience, tokenizer, testing, observability)
that isn't tied to a single capability.

| Label | Meaning |
|-------|---------|
| `scope: chat` | Chat/LLM completions capability. |
| `scope: embedding` | Embeddings capability. |
| `scope: reranking` | Reranking capability. |

## Cutting a release (maintainers)

Releases are automated; nobody edits a version by hand.

1. **Fill `[Unreleased]`** in `CHANGELOG.md` as you merge work (the release
   refuses an empty section).
2. **Run the "Prepare release" workflow** (Actions > Prepare release > Run
   workflow) with a bump: `auto` (the default) picks it from the conventional
   commits since the last tag (`type!:` or a `BREAKING CHANGE` footer: major;
   any `feat`: minor; otherwise patch), or choose `patch`, `minor`, `major`, or
   an exact `X.Y.Z`. It runs `scripts/prepare-release.sh`, which in one step
   bumps `[workspace.package] version` in `Cargo.toml`, the workspace crates
   in `Cargo.lock`, `info.version` in `docs/openapi.yaml`, and folds
   `[Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD` (with the compare-link
   footer). It then opens or refreshes the PR `chore(release): vX.Y.Z` from
   `release/vX.Y.Z`, with the new CHANGELOG section as its body, and dispatches
   CI on that branch.
3. **Merge the release PR** on green. `release-on-merge.yml` tags the merge
   commit `vX.Y.Z` and runs `release.yml` on it: the full test gate runs on
   the tagged commit, a **draft** release is created from the tag's CHANGELOG
   section (the job fails if the section is missing), both musl tarballs and
   their `.sha256` checksums attach to the draft, the multi-arch image goes to
   GHCR, a final `publish` job verifies all four assets before flipping the
   draft public, and `notify-lab` sends `lumen-released` to `meilisearch/lab`
   so it re-vendors `docs/openapi.yaml` (only when `LAB_DISPATCH_TOKEN` is set;
   otherwise the Lab's daily contracts run picks it up).

Locally, `scripts/prepare-release.sh` with no argument prints the suggested
bump without changing anything; `scripts/prepare-release.sh patch` (or
`minor`, `major`, `auto`, `X.Y.Z`) prepares the same commit the workflow makes,
and re-running it is safe (bumps count from the last tag).
`scripts/prepare-release.sh --check` is the CI `version sync` job: it fails
when `Cargo.toml`, `Cargo.lock` and `docs/openapi.yaml` disagree.

Why the tag is pushed and released by one workflow: a tag pushed with the
default `GITHUB_TOKEN` does not trigger other workflows, so
`release-on-merge.yml` calls `release.yml` as a reusable workflow instead of
relying on the tag push. No PAT is needed. Pushing a `vX.Y.Z` tag by hand still
triggers `release.yml` directly (for a release PR merged while the automation
was broken, for example).

Repository setup: Settings > Actions > General > "Allow GitHub Actions to
create and approve pull requests" must be on for the release PR. The optional
`LAB_DISPATCH_TOKEN` secret is a fine-grained token with Contents read and
write on `meilisearch/lab`.

**Releases are immutable.** Two consequences, both learned the hard way:

- Once published, a release cannot be edited and no asset can be added or
  replaced. That is why the pipeline is draft-first; keep it that way.
- Deleting a published release **permanently retires its tag name**: GitHub
  refuses to ever recreate that tag (v0.3.0 is burned this way). If a release
  ships broken, do not delete it and re-tag; fix the cause and cut the next
  patch version instead.

## Security

Please do not open public issues for vulnerabilities - see [`SECURITY.md`](SECURITY.md)
for responsible disclosure.
