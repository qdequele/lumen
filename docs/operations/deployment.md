# Deployment

## Docker

```bash
docker run -p 8080:8080 \
  -v ./config.toml:/config.toml \
  -e OPENAI_API_KEY=sk-... \
  ghcr.io/qdequele/lumen:latest
```

The image is built from a multi-stage `Dockerfile`: a static musl binary
copied onto a `distroless/static` non-root base - no shell, no libc in the
final image, just the gateway. It is multi-arch (`linux/amd64` and
`linux/arm64`). The image sets `LUMEN_SERVER__HOST=0.0.0.0` for you, so the
server binds to all interfaces inside the container; mount your config at
`/config.toml` (the image's default `CMD`).

## Bare binary

Static musl binaries for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` are attached to every GitHub release cut from a
`v*` tag - a single self-contained file, no runtime dependencies, which
makes it systemd-friendly as a single process:

```bash
lumen --config /etc/lumen/config.toml
```

Bind the host/port via `[server]` in the config file, or the
`LUMEN_SERVER__HOST` / `LUMEN_SERVER__PORT` environment variables.

## TLS

LUMEN intentionally does not terminate TLS. Put a reverse proxy (nginx,
Caddy, your load balancer) in front of it, and leave HSTS to that proxy.
The gateway speaks plain HTTP and should not be exposed directly to the
internet without one.

Every response does carry a conservative set of default security headers:

- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- `Referrer-Policy: no-referrer`
- `Content-Security-Policy: default-src 'none'`

## Surface control

- **Restrict `/admin/*` and `/metrics`** at the network layer (firewall,
  reverse proxy, service mesh) as appropriate for your deployment.
  `/admin/*` requires the master key, but `/metrics` is unauthenticated by
  design - see [`SECURITY.md`](https://github.com/qdequele/lumen/blob/main/SECURITY.md).
- **`GET /health`** is safe to point a liveness probe at: it never depends
  on provider state and does no I/O.

## Hot reload

A `SIGHUP` or a file-watch event triggers a config reload: the new file is
validated first, and only then is the provider registry atomically swapped
in. In-flight requests are unaffected. If the new config is invalid, it is
**rejected** - the old config keeps serving, and
`lumen_config_reload_failures_total` increments so the failed reload is
visible in your dashboards.

## Validate configs in the pipeline

Run `lumen --check-config` in CI or your deploy pipeline before a real boot.
It performs the same parsing, semantic validation and provider registry
construction the server does at startup, then exits `0` if the config is
valid and non-zero otherwise - without binding a listener, opening a
database, or contacting a provider. See
[Installation](../getting-started/installation.md#validate-a-config-without-booting).
