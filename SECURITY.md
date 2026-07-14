# Security Policy

## Reporting a vulnerability

Please report security issues **privately**, not via public GitHub issues.

- Use GitHub's **[Report a vulnerability](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)**
  (Security → Advisories → *Report a vulnerability*) on this repository, or
- email the maintainers at the address in the repository metadata.

Include a description, affected version/commit, and a reproduction if possible.
We aim to acknowledge within 3 business days and to ship a fix or mitigation
before any public disclosure. Please give us a reasonable window to remediate.

## Supported versions

LUMEN is pre-1.0. Security fixes land on `main` and in the latest tagged
release. There is no long-term-support branch yet.

## Security model - what LUMEN guarantees

LUMEN is a self-hosted gateway; you run it inside your own trust boundary.
The design makes a few guarantees relevant to security:

- **Secrets never leak.** Provider API keys are referenced by environment-variable
  *name* in config, never stored as values; when stored in the database (admin
  API) they are encrypted at rest with AES-256-GCM under `LUMEN_MASTER_KEY`.
  Keys are never logged, never placed in an error returned to a client, and the
  redacting `Debug` impls keep them out of debug output (enforced by tests).
- **Prompts and responses are never logged by default.** The usage log records
  token counts, cost and metadata labels - never message content.
- **Virtual keys** are stored only as BLAKE3 hashes; the plaintext is shown once
  at creation and never again. Unknown, disabled and expired keys are
  indistinguishable to the caller (`LM-4004`) so key state cannot be probed.
- **Hard budgets are enforced in memory before any upstream call**, so a
  rejected request never spends. Enforcement cannot be outrun by a crash.
- **Errors never mislead.** A client error, an upstream provider error, and an
  internal malfunction are always distinguished (4xx / 502-504 / 500); an
  internal failure is never reported as a 401.
- **Default response security headers**: `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, and a locked-down
  `Content-Security-Policy: default-src 'none'`. HSTS is intentionally left to
  the TLS-terminating proxy.

## Operator responsibilities

- **Terminate TLS** in front of LUMEN (a reverse proxy / load balancer). The
  gateway speaks plain HTTP; do not expose it directly to the internet without
  TLS.
- **Protect `LUMEN_MASTER_KEY`** and the SQLite database file - together they
  decrypt any stored provider keys.
- **Restrict `/admin/*` and `/metrics`** at the network layer as appropriate;
  `/admin/*` requires the master key, but metrics are unauthenticated by design.
- Keep dependencies current: CI runs `cargo audit` and `cargo deny` on every
  build.
