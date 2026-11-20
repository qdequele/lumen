<!-- One paragraph: what changes and why. Link the issue: Fixes #NN. -->

## Changelog

<!--
The Lab reads this section when the PR is merged and turns it into entries of the
customer changelog (an LLM rewrites them for customers; staff review the big ones).
One line per change customers will notice, with its kind:
  - major: a big new capability worth announcing to every customer
  - feature: a smaller new feature
  - improvement: an existing feature got better
  - fix: a bug fix
Write "none" for refactors, tests, CI, dependency bumps and staff-only changes.
-->

- feature: 

## Screenshots or video

<!--
Required when the change is visible in the console or is a major one: drag a
screenshot or a short screen recording (GIF/MP4) here. The changelog, the sidebar
announcement and the newsletter use it, so show the result, not the diff.
Write "n/a" when nothing is visual.
-->

## Definition of Done

Validation, must pass locally before opening the PR:

- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` (pedantic)
- [ ] `cargo fmt --check`

For any change touching source:

- [ ] Unit tests + at least one integration test (wiremock for providers)
- [ ] Cancellation tested if the change touches the request path
- [ ] A test proving no secret leaks into logs, if secrets are involved
- [ ] Doc comments on any new public API
- [ ] User-visible change noted in `CHANGELOG.md` under `[Unreleased]`
- [ ] No em-dashes anywhere, commit messages included (CI enforces the file check)
- [ ] Architecture choice not covered by an ADR? Write `docs/adr/NNN-title.md` first
