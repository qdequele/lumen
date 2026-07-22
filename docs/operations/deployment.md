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

### systemd unit

A minimal hardened unit. The two numbers that matter: `TimeoutStopSec` must
exceed the gateway's 30 s drain window (see
[Shutdown and restarts](#shutdown-and-restarts)), and `ReadWritePaths` must
cover the auth database directory when auth is enabled (set an absolute
`auth.db_path`; the default `lumen.db` is relative to the working
directory).

```ini
[Unit]
Description=LUMEN gateway
After=network-online.target
Wants=network-online.target

[Service]
User=lumen
Group=lumen
WorkingDirectory=/var/lib/lumen
ExecStart=/usr/local/bin/lumen --config /etc/lumen/config.toml
# Provider API keys and LUMEN_MASTER_KEY, mode 0600, never in the config.
EnvironmentFile=/etc/lumen/env
# SIGHUP = config hot reload (see below). systemd's default stop signal is
# SIGTERM, which is the graceful-drain path.
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
# The gateway drains in-flight requests for up to 30 s on SIGTERM, then
# runs a final accounting flush with a bounded wait of up to 5 s; give the
# whole clean path headroom before systemd escalates to SIGKILL.
TimeoutStopSec=40
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/lumen

[Install]
WantedBy=multi-user.target
```

## TLS and the reverse proxy

LUMEN intentionally does not terminate TLS. Put a reverse proxy (nginx,
Caddy, your load balancer) in front of it, and leave HSTS to that proxy.
The gateway speaks plain HTTP and should not be exposed directly to the
internet without one.

Caddy needs two lines (automatic HTTPS, streams flush correctly by
default):

```text
gateway.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

nginx needs response buffering off, or SSE streams arrive in bursts
instead of token by token, and a read timeout longer than your slowest
stream:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_read_timeout 300s;
}
```

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

## Scaling and high availability

The honest answer to "can I run two replicas behind the load balancer?" is:
**it depends on whether auth is enabled**, and v1 does not paper over that.

**With `[auth].enabled = false`** (the default), the gateway is a stateless
proxy: no database, no keys, no budgets. Run as many replicas as you like;
nothing breaks. Two per-instance caveats remain: circuit breakers and
health probes are per-process (each replica discovers a bad upstream on its
own), and `/metrics` is per-instance (scrape every replica and aggregate in
PromQL - the counters sum correctly).

**With `[auth].enabled = true`, v1 is single-instance by design.** Hard
budgets and RPM/TPM quotas are enforced **in per-process memory** (that is
what keeps the database off the request path), and spend is flushed to a
**per-node SQLite file**. Behind a load balancer, N replicas each enforce
the full budget and quota independently: a `$100` hard budget becomes an
effective `$100 x N`, an `rpm_limit` of 60 becomes `60 x N`, and the usage
ledger splits into N disjoint database files. Nothing crashes - the
guarantees silently stop meaning what they say, which is worse.

Until then, the supported shapes with auth enabled are:

- **One active instance.** A supervisor (systemd, a single-replica
  Kubernetes Deployment with `strategy: Recreate`) restarts it; the drain
  semantics below bound the restart blip to seconds.
- **Active/passive**: a standby instance behind a failover VIP or LB health
  check, sharing nothing. On failover the standby starts from its own
  (empty or restored) database; budgets re-enforce from the last flushed
  state of whatever database it opens.

A shared Postgres backend for the auth/usage store and distributed rate
limiting are the v2 items that lift this constraint - see
[the backlog](../backlog.md).

## Hot reload

A `SIGHUP`, a file-watch event, or an admin provider-key rotation
(`PUT /admin/provider-keys/{name}`) triggers a config reload: the new config
is validated first, and only then are the provider registry, price table,
resilience policy and the runtime-safe `[auth]` knobs
(`flush_interval_ms`, `retention_days`) atomically swapped in. Every reload
also re-reads DB-stored provider keys, so a key rotated via the admin API
takes effect without a restart even without an explicit trigger call; a DB
read error keeps the previous snapshot rather than stripping a working key.
In-flight requests are unaffected. If the new config is invalid, it is
**rejected** - the old config keeps serving, and
`lumen_config_reload_failures_total` increments so the failed reload is
visible in your dashboards.

Some settings stay boot-time only and need a real restart: the bind
address, `auth.enabled`, `auth.db_path`, and the bounded usage-log channel
knobs (`usage_channel_capacity`, `usage_batch_max`, `usage_flush_ms`) -
rebinding a live listener or resizing a running channel is out of scope for
a live swap.

## Shutdown and restarts

What each signal does:

| Signal | Effect |
|---|---|
| `SIGTERM` / `SIGINT` | Graceful shutdown: stop accepting, drain in-flight requests (SSE streams included) for up to **30 seconds**, then exit. |
| `SIGHUP` | Config hot reload (above). **Not** a shutdown. |

The drain window is a built-in constant, not configurable, and a clean
stop can spend up to 5 more seconds on the final accounting flush below.
Tune your supervisor against the whole path: systemd `TimeoutStopSec=40`
(the unit above), Kubernetes `terminationGracePeriodSeconds: 40`. A
supervisor that kills sooner turns graceful restarts into the crash case
below; if draining ever exceeds 30 s, the gateway logs a warning and exits
anyway rather than hanging.

Accounting across a stop, when auth is enabled:

- **Clean shutdown attempts a final accounting flush with a bounded wait.**
  After the listener drains, the gateway performs a final budget flush and
  waits up to 5 seconds for the usage-log writer to drain its channel.
  Usage rows still in the channel when the wait expires are lost, as are
  rows from database write failures (both logged as warnings rather than
  blocking exit). In the common case, a clean shutdown loses no accounting.
- **A crash loses at most `flush_interval_ms`** (default 10 s) of budget
  *accounting* and whatever usage-log rows were still in the bounded
  channel. Budget enforcement itself lives in memory ahead of the flush, so
  a running process never allows overruns. After a restart, however,
  budgets reload from the last persisted state: unflushed usage lost in a
  crash can permit spend beyond the intended budget until the gap closes.

Rolling restarts through the reverse proxy work as expected: mark the
instance down (or just send `SIGTERM`), let the 30 s drain finish the
in-flight streams, start the new binary. With auth enabled, avoid running
old and new **concurrently** for long - see
[Scaling and high availability](#scaling-and-high-availability).

## Backups

Everything durable lives in one SQLite file: `auth.db_path` (default
`lumen.db`). It is the **only copy** of the virtual-key hashes, the
encrypted provider keys stored via the admin API, the budget state, and
the entire `usage_log` ledger. With auth disabled there is no database and
nothing to back up.

- **Live backup** (server running): the database runs in WAL mode, so use
  SQLite's online backup rather than copying the file:

  ```bash
  sqlite3 /var/lib/lumen/lumen.db ".backup '/backups/lumen-$(date +%F).db'"
  ```

  A live backup can trail reality by up to `flush_interval_ms` (default
  10 s) of budget accounting - the in-memory spend not yet flushed.
- **Cold backup** (consistent snapshot): stop the gateway first (a clean
  `SIGTERM` attempts a final flush, see above), then copy `lumen.db`
  together with its `-wal` and `-shm` sidecar files if present. The backup
  reflects the state at shutdown, though in-flight or unflushed usage rows
  from the bounded channel may not be included.
- **Restore needs the matching `LUMEN_MASTER_KEY`.** Stored provider keys
  are encrypted under it; a restored database without the same master key
  serves virtual keys and history fine, but every stored provider key is
  undecryptable (re-enter them via the admin API). Per
  [SECURITY.md](https://github.com/qdequele/lumen/blob/main/SECURITY.md),
  the key and the database are a pair: back up and protect them together,
  but never in the same place.
- Virtual keys are stored as BLAKE3 hashes and the plaintext is shown only
  once at creation: **a lost database is unrecoverable key-wise**. Clients
  keep their plaintext keys, but the gateway no longer knows them; they
  must be re-created. Back up on a schedule that matches how much
  `usage_log` history you are willing to lose.

## Validate configs in the pipeline

Run `lumen --check-config` in CI or your deploy pipeline before a real boot.
It performs the same parsing, semantic validation and provider registry
construction the server does at startup, then exits `0` if the config is
valid and non-zero otherwise - without binding a listener, opening a
database, or contacting a provider. See
[Installation](../getting-started/installation.md#validate-a-config-without-booting).
