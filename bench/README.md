# LUMEN benchmark harness

Reproducible head-to-head: **LUMEN vs LiteLLM** in front of the *same* local
mock upstream, so the difference measured is proxy overhead, not model time.
See `docs/perf-baseline.md` for methodology and the recorded in-process numbers.

## Layout

- `compose.yaml` - mock upstream (mockserver, ~0 ms), LUMEN, and LiteLLM,
  all pointed at the mock. Third-party images are pinned by tag + digest.
- `mock-upstream.json` - the constant OpenAI-shaped response, registered at
  both `/chat/completions` (what the gateways call on the upstream) and
  `/v1/chat/completions` (what the direct-baseline k6 run calls on the mock
  itself).
- `lumen-bench.toml` / `litellm-bench.yaml` - matching single-model configs.
- `k6-added-latency.js` - 50-VU, 30 s constant load reporting p50/p95/p99.
- `run.sh` - drives the whole harness end to end (build, wait for readiness,
  run k6 against all three targets, sample RAM under load, write a report)
  and writes a timestamped, self-contained result under `results/`.
- `results/<UTC timestamp>/` - committed output of a `run.sh` invocation:
  `report.md` (the human-readable summary), the raw k6 `*.summary.json` /
  `*.log`, and the `docker stats` captures. Each run gets its own directory
  so history accumulates instead of being overwritten; `docs/perf-baseline.md`
  links the latest one.

## Run

The one-command path:

```bash
bench/run.sh
```

This builds/starts the stack, runs k6 against the direct mock, LUMEN, and
LiteLLM in turn, samples `docker stats` ~10 s into each gateway run, tears
the stack down, and writes `bench/results/<timestamp>/report.md`. Requires
`docker` (with compose v2), `k6`, and `jq` on `PATH`.

To drive the pieces by hand instead:

```bash
docker compose -f bench/compose.yaml up -d --build

# Percentiles for each target (run one at a time so they don't contend):
OPENAI_API_KEY=sk-mock TARGET=http://localhost:1080 k6 run bench/k6-added-latency.js  # direct (baseline)
OPENAI_API_KEY=sk-mock TARGET=http://localhost:8080 k6 run bench/k6-added-latency.js  # lumen
OPENAI_API_KEY=sk-mock TARGET=http://localhost:4000 k6 run bench/k6-added-latency.js  # litellm

# Memory + CPU while a run is in flight:
docker stats --no-stream mock-upstream lumen litellm

docker compose -f bench/compose.yaml down -v
```

## Reading the results

- **Added latency** = gateway `http_req_duration` percentile − direct percentile,
  at p50 and p99. The direct baseline captures the localhost hop + mock time so
  the subtraction isolates the gateway.
- **RAM** = the `MEM USAGE` column from `docker stats` for `lumen` vs
  `litellm` under the same load.
- **Throughput** = k6's `http_reqs` rate (req/s) for each target.

The mock returns instantly, so absolute latencies are tiny and dominated by
transport - which is exactly the point: it exposes the proxy's own overhead
rather than hiding it behind model latency. The k6 script's `p(99)<50ms`
threshold is a sanity guard against something being pathologically broken,
not a pass/fail gate for the harness itself - `run.sh` records and reports
the real percentiles even when a busy host blows through it (see the
"Environment" section of each `results/*/report.md` for what host produced
those numbers).

> Note: the first run needs network access to pull the LiteLLM and
> mockserver images and a few minutes to build LUMEN's release binary.
> Absolute numbers are hardware- and host-load-dependent; the *relative*
> comparison (LUMEN vs LiteLLM against the same mock, same host, same run)
> is what's meaningful across environments. Re-run `bench/run.sh` on your
> own target hardware for numbers you can rely on operationally.

## Updating the pinned versions

`compose.yaml` pins `mockserver/mockserver` and `ghcr.io/berriai/litellm` by
tag *and* digest. To refresh either:

1. Pick the new tag (check the [LiteLLM releases page](https://github.com/BerriAI/litellm/releases)
   for the latest non-prerelease version).
2. `docker pull <image>:<new-tag>` and read the resolved digest off
   `docker inspect <image>:<new-tag> --format '{{index .RepoDigests 0}}'`.
3. Update the `image:` line in `compose.yaml` to `<image>:<new-tag>@<digest>`.
4. Re-run `bench/run.sh` and commit the new `results/<timestamp>/` alongside
   the version bump, and update the link in `docs/perf-baseline.md`.
