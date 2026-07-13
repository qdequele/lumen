# Ferrogate benchmark harness

Reproducible head-to-head: **Ferrogate vs LiteLLM** in front of the *same* local
mock upstream, so the difference measured is proxy overhead, not model time.
See `docs/perf-baseline.md` for methodology and the recorded in-process numbers.

## Layout

- `compose.yaml` — mock upstream (mockserver, ~0 ms), Ferrogate, and LiteLLM,
  all pointed at the mock.
- `mock-upstream.json` — the constant OpenAI-shaped `/chat/completions` response.
- `ferrogate-bench.toml` / `litellm-bench.yaml` — matching single-model configs.
- `k6-added-latency.js` — 50-VU, 30 s constant load reporting p50/p95/p99.

## Run

```bash
docker compose -f bench/compose.yaml up -d --build

# Percentiles for each target (run one at a time so they don't contend):
OPENAI_API_KEY=sk-mock TARGET=http://localhost:1080 k6 run bench/k6-added-latency.js  # direct (baseline)
OPENAI_API_KEY=sk-mock TARGET=http://localhost:8080 k6 run bench/k6-added-latency.js  # ferrogate
OPENAI_API_KEY=sk-mock TARGET=http://localhost:4000 k6 run bench/k6-added-latency.js  # litellm

# Memory + CPU while a run is in flight:
docker stats --no-stream mock-upstream ferrogate litellm
```

## Reading the results

- **Added latency** = gateway `http_req_duration` percentile − direct percentile,
  at p50 and p99. The direct baseline captures the localhost hop + mock time so
  the subtraction isolates the gateway.
- **RAM** = the `MEM USAGE` column from `docker stats` for `ferrogate` vs
  `litellm` under the same load.
- **Throughput** = k6's `http_reqs` rate (req/s) for each target.

The mock returns instantly, so absolute latencies are tiny and dominated by
transport — which is exactly the point: it exposes the proxy's own overhead
rather than hiding it behind model latency.

> Note: `docker compose up` pulls the LiteLLM image and builds Ferrogate; the
> first run needs network and a few minutes. The numbers are hardware-specific —
> record your environment alongside them (see `docs/perf-baseline.md`).
