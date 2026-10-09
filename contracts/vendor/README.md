# Vendored contracts

Byte copies of contracts owned by other repositories. Never edit them here:
change the file in the owner repository, then copy it over the vendored one.
CI (`lab-contract-drift` in `.github/workflows/ci.yml`) fails when a copy
differs from the owner's `main`; it needs the `LAB_REPO_TOKEN` secret (a
read-only token on `meilisearch/lab`) and skips with a notice without it.

| File | Owner | What |
|------|-------|------|
| `lab/lab-events.schema.json` | `meilisearch/lab` `contracts/lab-events.schema.json` | The events LUMEN sends to `POST {LAB_URL}/internal/events` (platform contract v2, section 4). The serializer in `crates/auth/src/billing.rs` is tested against it. |
