# LUMEN vs LiteLLM benchmark run - 20260715T231135Z

Reproduced with `bench/run.sh` (see `bench/README.md` for methodology
and `docs/perf-baseline.md` for how this fits the performance pillar).

## Environment

| | |
|---|---|
| Host | Darwin arm64 |
| Docker | 29.4.0 |
| k6 | k6 v2.0.0 (commit/devel, go1.26.3, darwin/arm64) |
| Git commit | 51fc809 |

Pinned images actually run (tag + resolved digest):

```
mockserver/mockserver:5.15.0@sha256:0f9ef78c94894ac3e70135d156193b25e23872575d58e2228344964273b4af6b
ghcr.io/berriai/litellm:v1.92.0@sha256:9ef6f45bc0104940571765e610c52a1d761b5ec85efcd193795281086ee61277
```

## Results

| Target | p50 (ms) | p95 (ms) | p99 (ms) | req/s |
|---|---|---|---|---|
| direct | 6.91 | 204.99 | 836.43 | 1191.7 |
| lumen | 9.44 | 36.57 | 220.57 | 2733.5 |
| litellm | 323.75 | 656.67 | 6490.29 | 111.3 |

Added latency = gateway percentile - direct percentile.

## RAM under load (`docker stats --no-stream`, sampled ~10s into each run)

```
CONTAINER   NAME      CPU %     MEM USAGE / LIMIT
b6f4cd83882a   lumen     99.00%    7.633MiB / 7.818GiB   0.10%     20.6MB / 24.8MB   14MB / 0B   10
c7e2daac328a   litellm   101.86%   1.028GiB / 7.818GiB   13.15%    667kB / 1.42MB   179MB / 56.7MB   17
```

## RAM at idle (post-load, `docker stats --no-stream`)

```
CONTAINER ID   NAME            CPU %     MEM USAGE / LIMIT     MEM %     NET I/O           BLOCK I/O        PIDS
b6f4cd83882a   lumen           0.00%     13.78MiB / 7.818GiB   0.17%     67.2MB / 81.2MB   23.9MB / 0B      10
c7e2daac328a   litellm         0.26%     1.037GiB / 7.818GiB   13.26%    3.07MB / 7.71MB   179MB / 59.8MB   17
05c1caab14ee   mock-upstream   105.82%   833.8MiB / 7.818GiB   10.42%    43.2MB / 62.3MB   138MB / 233kB    43
```
