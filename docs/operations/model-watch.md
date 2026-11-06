# Model release watcher

LUMEN connectors are per provider, not per model. A new model on a provider
LUMEN already supports only needs a `[[providers.models]]` entry; a new
provider, or a new API shape, needs code in `crates/providers`. The model
release watcher tells maintainers which of the two each new release is, so
neither goes unnoticed.

It is a scheduled GitHub Actions workflow
([`.github/workflows/model-watch.yml`](https://github.com/qdequele/lumen/blob/main/.github/workflows/model-watch.yml))
running a standard-library Python script
([`scripts/model-watch/model_watch.py`](https://github.com/qdequele/lumen/blob/main/scripts/model-watch/model_watch.py)).
It needs no secrets: every source is public, and it files issues with the
workflow's own `GITHUB_TOKEN`. It is a repository maintenance tool; it never
runs inside the gateway.

## What it does

Daily (06:17 UTC) and on manual dispatch, it:

1. **Fetches three public catalogs**: [models.dev](https://models.dev/api.json),
   LiteLLM's
   [`model_prices_and_context_window.json`](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json),
   and OpenRouter's [`/api/v1/models`](https://openrouter.ai/api/v1/models)
   plus `/api/v1/embeddings/models`. LiteLLM's `openai_like/providers.json`
   is read as a hint for which of its providers speak the OpenAI format.
2. **Diffs them** against the committed snapshot,
   `scripts/model-watch/snapshot.json`: per source, provider id to model id to
   capability (`chat`, `embedding` or `rerank`; image, audio and video models
   are ignored).
3. **Classifies every addition** against what LUMEN supports, read at run
   time from `crates/providers/src/kind.rs` (the `ProviderKind` variants,
   `is_openai_compatible`, `default_base_url`, `supports_embeddings`) and the
   capability tables in [`docs/providers.md`](../providers.md):
   - **(a) config only**: a new model on a provider LUMEN supports. If the
     kind does not serve that capability yet (an embedding model on an
     embed-less kind, say), it is listed as a **capability gap** instead.
   - **(b) OpenAI-compatible provider**: a provider LUMEN does not know whose
     catalog entry declares an OpenAI-compatible base URL. Candidate for a new
     `ProviderKind` variant in the OpenAI-compatible family.
   - **(c) needs a connector**: a provider with a native API (or one the
     catalogs cannot vouch for), or any rerank model from an unsupported
     provider, since rerank has no OpenAI wire format.
4. **Files issues**, labelled per [CONTRIBUTING.md](../contributing.md):
   - one issue per (b)/(c) provider, titled `Provider candidate: <name>
     [model-watch:provider:<id>]`, with the catalog ids, base URL and a
     next-steps checklist. Labels: `enhancement`, `area: providers`,
     `priority: medium` when it brings embedding or rerank models (otherwise
     `priority: low`), and the matching `scope:` labels. A later run comments
     on the open issue instead of opening another.
   - one rolling issue, `New models on supported providers
     [model-watch:supported-models]`, for every (a) addition, grouped by
     LUMEN kind. Each run adds a dated comment.
5. **Commits the refreshed snapshot** to the branch it ran on, only after
   every issue was filed.

Providers that were already in the catalogs when the snapshot was first
taken are the baseline: the watcher stays quiet about their new chat models
(many are aggregators reselling the same models), and only speaks up when
they add an embedding or rerank model.

## Triage

- **Close a provider issue** once handled, or as `wontfix` to silence it.
  The watcher never reopens a closed provider issue.
- **Close the rolling issue** after the batch is triaged; the next run with
  new models opens a fresh one.
- **Teach it a mapping**: when a catalog spells a supported provider
  differently (`together_ai` for `together`), add the id to
  `scripts/model-watch/providers.json`. Ids are normalized (lowercase,
  separators removed) and a provider whose base URL equals a kind's
  `default_base_url` is matched automatically. A capability only described in
  prose in `docs/providers.md` (Together and Cloudflare rerank) goes under
  `extra_capabilities` there.
- **A new `ProviderKind`** is picked up by the next run with no watcher
  change, as long as it is also added to the `docs/providers.md` tables (the
  run summary warns about a kind missing from them).

## Running it locally

```bash
python3 scripts/model-watch/model_watch.py --dry-run                 # what would be filed today
python3 scripts/model-watch/model_watch.py --dry-run --show-bodies   # with full issue bodies
python3 -m unittest discover -s scripts/model-watch                  # unit tests
```

`--dry-run` never writes to GitHub or to the snapshot. With `GITHUB_TOKEN`
and `GITHUB_REPOSITORY` set, it also reads existing watcher issues, so it
reports `comment` and `skip` where a real run would. `--source-dir DIR` reads
`<source>.json` files from `DIR` instead of fetching (offline replay).
`--init` rebuilds the snapshot from today's catalogs without filing anything,
for example after a deliberate catalog id migration.

## Failure handling

- **A catalog changes shape** or is unreachable: that source is skipped, its
  previous snapshot section is kept, the healthy sources are still processed,
  and the run goes red. Adapt the matching `parse_*` function.
- **A catalog shrinks** below half its previous size, or below a sane
  minimum: treated the same way, so an outage page is never recorded as a
  mass removal (which would re-announce everything on recovery).
- **Too many new issues** in one run (more than `--max-new-issues`, default
  10; a dispatch input): nothing is filed and the snapshot is left as is.
  This is almost always a catalog renaming its ids. Inspect with
  `--dry-run`, then either add aliases or re-dispatch with a higher limit.
- **Issue filing fails midway**: the snapshot is not written, so the next run
  re-detects the same additions (an open issue may receive the same comment
  twice).
- **Schedule paused**: GitHub disables scheduled workflows after 60 days
  without repository activity. Re-enable it from the Actions tab; the
  committed snapshot makes the first run after a pause catch up in one batch.

## Why the snapshot is committed

The snapshot is the watcher's only state, and it is committed back to `main`
rather than kept as a workflow artifact. Artifacts expire (90 days at most),
and a lost snapshot means either refiling the whole catalog or silently
skipping a window of releases. A committed file survives pauses, is
reviewable with `git log -p`, and lets anyone reproduce a run locally with
`--dry-run`. The cost is one small bot commit on days the catalogs change;
`main` only blocks deletions and force-pushes, so the workflow's token can
push it, and commits made with that token do not trigger other workflows.

## Future work

Not built yet: a later phase could hand type (c) issues to a Claude Code
GitHub Action running the `provider-integrator` agent to draft a connector
PR, always for human review. The watcher only detects and triages; it never
writes provider code.
