# LUMEN real-life test rig

One-command stack to run the gateway against **eight real providers** (OpenAI,
Anthropic, Mistral, Google Gemini, Cohere, Jina, Voyage, Cloudflare Workers AI)
with Prometheus scraping it and a pre-provisioned Grafana dashboard showing the
consumption: token rates per provider/model/capability/direction, rerank search
units, media accounting, circuit-breaker state, and gateway internals.
Prometheus also loads the starter alert rules from
[`prometheus/alerts.yml`](prometheus/alerts.yml) (usage-log drops, rejected
reloads, open breakers, probe-down providers, 5xx ratio); watch them fire at
<http://localhost:9090/alerts>.

## 1. Put your keys in `.env`

```bash
cd monitoring
cp .env.example .env
$EDITOR .env          # paste the keys you have; leave the rest empty
```

Every key is optional: the gateway reads a provider's key only when a request
routes there, and the test suite skips providers without one.

**Cloudflare only**: also paste your account id into [lumen.toml](lumen.toml)
(replace `YOUR_CLOUDFLARE_ACCOUNT_ID` in the `base_url`), then
`docker compose up -d --force-recreate lumen`.

`.env` is gitignored; keys never appear in the config file, in logs, or in git.

## 2. Start the stack

```bash
docker compose up -d --build
```

| Service | URL | Notes |
|---|---|---|
| Gateway | <http://localhost:8080> | OrbStack: `http://lumen.lumen-monitoring.orb.local` |
| Grafana | <http://localhost:3000> | `admin` / `lumen` (anonymous viewing enabled) |
| Prometheus | <http://localhost:9090> | 5s scrape interval |

The **LUMEN Gateway** dashboard (`/d/lumen-gateway`) is provisioned
automatically and set as the home dashboard.

Changed `.env` later? `docker compose up -d lumen` re-creates the gateway with
the new keys. Changed `lumen.toml`? Use
`docker compose up -d --force-recreate lumen` - compose does not detect
content changes inside a bind-mounted file.

## 3. Run the test suite

```bash
./smoke.py
```

Per provider it checks chat, chat **streaming**, embeddings and reranking
(whichever the provider serves), asserts each response is well-formed and
carries a **non-zero token count** (ADR 003), and prints a PASS/FAIL/SKIP
table. Exit code 1 if anything configured fails. Python 3.9+, stdlib only.

It also covers the advanced features:

- **Vision (media in chat)**: a `data:` URI image part on
  `/v1/chat/completions` through OpenAI (forwarded verbatim), Anthropic
  (translated to a base64 source block) and Gemini (translated to
  `inline_data`), asserting the model actually describes the image.
- **Function calling**: a full two-leg roundtrip on OpenAI, Anthropic, Mistral
  and Gemini - the model must emit a `get_weather` `tool_calls` response
  (`finish_reason: "tool_calls"`, arguments parsed), then ground its final
  answer in the tool result we send back. Streamed `tool_calls` deltas are
  checked on OpenAI.
- **Multimodal embeddings (M9)**: a mixed batch (plain text item + text+image
  content-parts item) on Cohere embed-v4, then asserts the M9 media
  accounting appeared on `/metrics` (`lumen_media_total{capability="embed",
  media_type="image"}`).
- **Gateway guards** (pre-flight rejections, no upstream call): an image to a
  text-only model is `400 LM-2003`; a remote image URL routed to Gemini is
  `400 LM-2004` (the gateway never fetches user URLs - SSRF rule); a remote
  image URL on `/v1/embeddings` with `[image_fetch]` disabled is
  `400 LM-2005`.
- **Multi-tenant metadata**: the allowlisted org/team/project keys must come
  back as Prometheus labels.

## 4. Fill the dashboard

```bash
./traffic.py            # 10 min of randomized traffic, one request every 2s
./traffic.py 3600       # an hour
INTERVAL=1 ./traffic.py # faster
```

Both scripts tag every request with `x-lumen-metadata`, e.g.

```json
{"scenario":"traffic","org_id":"acme","team_id":"search","project_id":"website-search"}
```

`telemetry.metadata_labels = ["scenario", "org_id", "team_id", "project_id"]`
in `lumen.toml` allowlists exactly those keys as Prometheus labels (ADR 002:
non-allowlisted keys stay logs-only, so clients can never mint new time
series). The dashboard splits consumption on them: "Token rate by scenario"
separates smoke vs traffic, and the **multi-tenant row** shows token rate by
org, by team, and the top projects by tokens.

## Multi-tenant accounting

`traffic.py` simulates six tenants (three orgs, mixed teams/projects) by
rotating the `org_id` / `team_id` / `project_id` metadata per request; the
smoke suite runs as `acme / qa / smoke-suite` and asserts the labels land on
`lumen_tokens_total`. To attribute real traffic, have each caller send its own
ids in the header:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-lumen-metadata: {"org_id":"acme","team_id":"rag","project_id":"docs-chat"}' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
```

Keep the value sets bounded: every distinct org/team/project combination is a
Prometheus time series. Per-tenant queries then compose naturally, e.g. tokens
per org over 24h:
`sum by (org_id) (increase(lumen_tokens_total{org_id!=""}[24h]))`.

## What the dashboard shows (and where it comes from)

| Panel | Metric |
|---|---|
| Token totals, rates by provider / model / capability / direction | `lumen_tokens_total{capability,model,provider,direction,estimated}` |
| Locally estimated share | `estimated="true"` slice + `lumen_tokens_estimated_total` |
| Rerank search units | `lumen_rerank_search_units_total{model,provider}` |
| Media items / decoded bytes | `lumen_media_total`, `lumen_media_bytes_total` (M9) |
| API latency by endpoint (p50/p99), request rate by endpoint/status | `lumen_http_request_duration_seconds{method,path,status}` (path = matched route template) |
| End-to-end latency by provider / model (p99) | `lumen_request_duration_seconds{capability,model,provider,status}` (streaming covers the whole stream) |
| Circuit breaker timeline | `lumen_circuit_state{provider,model}` (0 closed / 1 open / 2 half-open) |
| Provider health | `lumen_provider_up{provider}` (only providers with an explicit `base_url` are probed - here Cloudflare) |
| Internals | `lumen_usage_log_dropped_total`, `lumen_metadata_rejected_total`, `lumen_config_reloads_total`, `lumen_config_reload_failures_total` |

Per-request **cost** is not a Prometheus series: prices configured in
`lumen.toml` (`cost_per_1m_*`, `cost_per_1k_searches`) feed the `usage_log`
accounting and hard budgets when `[auth]` is enabled - see ADR 003.

## Watching resilience live

Kill one vendor to watch the fallback chain and circuit breaker on the
dashboard: put a bogus key in `.env` for `OPENAI_API_KEY`, restart the gateway
(`docker compose up -d lumen`), and send chat traffic to `gpt-4o-mini`. After
5 consecutive failures the circuit opens (red on the timeline) and requests are
served by `claude-haiku-4-5` / `mistral-small` - the `x-lumen-model-used`
response header names whichever model actually answered.

## Tear down

```bash
docker compose down          # keep Prometheus/Grafana data
docker compose down -v       # wipe it
```
