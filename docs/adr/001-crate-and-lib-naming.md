# ADR 001 - Bare package names, `lumen_*` library names

- Status: accepted
- Date: 2026-07-12

## Context

The `CLAUDE.md` architecture lays out a six-crate workspace under `crates/`
(`core`, `providers`, `router`, `auth`, `telemetry`, `server`) and documents
commands like `cargo run -p server`. That `-p server` selector requires the
Cargo **package** name to be the bare `server`.

Naming a package `core`, however, is hazardous: a library crate literally named
`core` lands in the extern prelude of any downstream crate and shadows the
standard library's `::core`. This surfaces in the doctest harness, where
`::core::fmt`, `::core::future`, etc. (referenced by expanded std/`async_trait`
macros) fail to resolve - observed concretely as `E0433: cannot find 'fmt' in
'core'` while normal builds still passed.

## Decision

Keep **package** names bare (`core`, `providers`, `router`, `auth`,
`telemetry`, `server`) so the documented `-p <name>` commands work, but give
each library crate an explicit **lib** name prefixed `lumen_`:

```toml
[package]
name = "core"

[lib]
name = "lumen_core"
path = "src/lib.rs"
```

Internal dependencies are wired in `[workspace.dependencies]` with the
`lumen-*` key mapped to the bare package via `package`:

```toml
lumen-core = { path = "crates/core", package = "core" }
```

So: `cargo run -p server` works, imports read `use lumen_core::…`, and no
crate shadows a std crate.

## Consequences

- `cargo run -p server -- --config …` (and `-p core`, etc.) match the docs.
- No std-crate shadowing anywhere, including doctests.
- Slight indirection: the `[workspace.dependencies]` key differs from the
  package name. Documented here so it is not mistaken for an accident.
- The published crate names (if we ever publish) would be the bare names; we
  can revisit and prefix them at publish time without touching source.
