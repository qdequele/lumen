# multi-tenant-analytics

Per-tenant cost and usage attribution: `[auth]` enabled with a virtual key
per tenant carrying a hard budget, and `[telemetry].metadata_labels` turning
the `x-lumen-metadata` header into Prometheus labels. Needs
`OPENAI_API_KEY` and `LUMEN_MASTER_KEY` (64 hex characters).

## Generate a master key

`LUMEN_MASTER_KEY` gates the `/admin/*` API and seals provider keys stored
at rest; it must be 64 hex characters (32 bytes):

```bash
export LUMEN_MASTER_KEY=$(openssl rand -hex 32)
```

## Run it

```bash
# terminal 1 - start the gateway with this scenario's config
export OPENAI_API_KEY=sk-...
cargo run -p server -- --config examples/multi-tenant-analytics/config.toml

# terminal 2 - fire the requests
export LUMEN_MASTER_KEY=...   # same value as terminal 1
./examples/multi-tenant-analytics/run.sh
```

`run.sh`:

1. Creates a virtual key for tenant `acme` via `POST /admin/keys`
   (`Authorization: Bearer $LUMEN_MASTER_KEY`) with a small `budget_max`.
2. Sends 2 chat requests with that virtual key, each carrying
   `x-lumen-metadata: {"org_id":"acme","team_id":"search","project_id":"docs"}`.
3. Scrapes `/metrics` and greps for `lumen_tokens_total` labelled
   `org_id="acme"`.

## What to look for on `/metrics`

Because `metadata_labels = ["org_id", "team_id", "project_id"]`, every
`lumen_tokens_total` sample for a request that carried those keys is
labelled with them, so token counts can be sliced per tenant:

```
lumen_tokens_total{...,org_id="acme",team_id="search",project_id="docs"} 42
```

See [Usage log & multi-tenant metadata](../../docs/operations/usage-log.md)
for the full metadata contract and
[Keys, quotas & budgets](../../docs/operations/keys-budgets.md) for the
admin API and budget enforcement.

## Notes

- `--check-config` on this scenario's config does **not** need
  `LUMEN_MASTER_KEY` set: it only parses and validates `config.toml`
  (providers, models, `[auth]` shape), it never touches the master key -
  that is read straight from the environment at actual server startup, in
  `main.rs`, not through the config loader. In fact, do **not** export
  `LUMEN_MASTER_KEY` before running `--check-config` on any scenario in this
  repository: the config loader merges every `LUMEN_`-prefixed environment
  variable into the config as a field (`figment`'s `Env::prefixed("LUMEN_")`),
  and `master_key` is not a recognized top-level config field, so a set
  `LUMEN_MASTER_KEY` makes `--check-config` fail with an "unknown field"
  error on every scenario, not just this one.
- The `db_path = "examples-multi-tenant.db"` SQLite file is created next to
  wherever the gateway is started from and is gitignored; delete it to reset
  the scenario's virtual keys and usage log.
