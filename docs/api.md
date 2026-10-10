# HTTP API reference

The gateway's HTTP contract is one OpenAPI 3.1 document,
[`docs/openapi.yaml`](https://github.com/qdequele/lumen/blob/main/docs/openapi.yaml)
(its `info.version` is the gateway version): the OpenAI-compatible `/v1`
surface (chat completions, embeddings, rerank, decisions, models; the
deprecated `/v1/systemone` alias of `/v1/decisions` is documented as such),
the operational routes (`/health`, `/health/providers`, `/metrics`) and
every master-key `/admin` route a control plane drives.

- A test in `crates/server/src/app.rs` fails the build when a mounted route
  is missing from the document or the document names a route that is not
  mounted, so the file always describes the running router.
- With `[auth] enabled = true` the gateway serves the same document as JSON
  at `GET /openapi.json` (master key), which is how the Meilisearch Lab
  vendors it. Like the other platform routes it answers `403 LM-4005` to a
  call scoped with `X-Lumen-Account-Ref` (see
  [Lab integration](operations/lab-integration.md#one-gateway-many-accounts)).
- Every error is the `{"error": {"code": "LM-xxxx", ...}}` envelope of
  [Error codes](errors.md).

Render it with any OpenAPI viewer, for example
`npx @redocly/cli@1 build-docs docs/openapi.yaml` (writes a static
`redoc-static.html`).
