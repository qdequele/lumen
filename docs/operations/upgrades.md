# Upgrades

An upgrade is: replace the binary (or pull the new image), restart. This
page covers the two things that make that boring in practice: the database
migration story and what version numbers promise.

## Before upgrading

1. **Read the target release's section in
   [`CHANGELOG.md`](https://github.com/qdequele/lumen/blob/main/CHANGELOG.md).**
   Release-specific upgrade notes live there (for example, 0.2.0 carries a
   one-line repair for databases created by 0.1.0, after a migration
   file's checksum changed).
2. **Back up the database** if auth is enabled - see
   [Backups](deployment.md#backups). Upgrades run schema migrations
   automatically, and the backup is your downgrade path.
3. **Validate your config against the new binary** before booting it for
   real: `lumen-new --check-config --config /etc/lumen/config.toml`. A
   removed or renamed config key fails here, in the pipeline, instead of
   at restart time.

## Schema migrations run themselves

When auth is enabled, the gateway applies its embedded, numbered SQLite
migrations **automatically at boot** (six of them as of 0.2.0). There is
no separate migrate command and nothing to run by hand.

- **Forward-only.** There are no down-migrations. Rolling back to an older
  binary against a database a newer binary already migrated is not
  supported: the older binary refuses to start when it finds applied
  migrations it does not know. The downgrade path is the pre-upgrade
  backup.
- **Integrity-checked.** Each applied migration's checksum is verified at
  boot; a mismatch is a hard, named error rather than a silent
  divergence.
- With `[auth].enabled = false` there is no database and this whole
  section is moot.

## The restart itself

`SIGTERM` the old process (or let your supervisor do it), start the new
one. In-flight requests get up to 30 seconds to finish and a clean
shutdown attempts a final accounting flush with a bounded wait (up to 5
seconds) - the mechanics and the supervisor timeouts to pair with them are
in [Shutdown and restarts](deployment.md#shutdown-and-restarts). With auth
enabled, prefer stop-then-start over running old and new side by side; two
live instances double-enforce budgets and quotas for as long as they
overlap (see
[Scaling and high availability](deployment.md#scaling-and-high-availability)).

## What version numbers promise

LUMEN is **pre-1.0** and follows SemVer's 0.x rules: a minor bump
(`0.2 -> 0.3`) may contain breaking changes; a patch bump does not. Every
breaking change is called out explicitly in the CHANGELOG. Per surface:

- **HTTP API**: the OpenAI-compatible (`/v1/chat/completions`,
  `/v1/embeddings`) and Cohere-compatible (`/v1/rerank`) surfaces track
  those upstream formats; gateway-specific behavior changes are CHANGELOG
  items.
- **`LM-xxxx` error codes are stable identifiers.** Codes are never
  renumbered or reused; new ones are added and documented in
  [the error reference](../errors.md).
- **Config format**: keys can be added in any release; removals or renames
  are breaking changes (CHANGELOG + caught by `--check-config`, which
  rejects unknown keys).
- **Metrics**: renaming or re-labeling an exported series is a breaking
  change for your dashboards and is called out in the CHANGELOG.
