<!-- One paragraph: what changes and why. Link the issue: Fixes #NN. -->

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
