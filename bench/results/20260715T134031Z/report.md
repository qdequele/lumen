# LUMEN vs LiteLLM benchmark run - 20260715T134031Z

Reproduced with `bench/run.sh` (see `bench/README.md` for methodology
and `docs/perf-baseline.md` for how this fits the performance pillar).

## Environment

| | |
|---|---|
| Host | Darwin arm64 |
| Docker | 29.4.0 |
| k6 | k6 v2.0.0 (commit/devel, go1.26.3, darwin/arm64) |
| Git commit | 243ff0d |

Pinned images actually run (tag + resolved digest):

```
mockserver/mockserver:5.15.0@sha256:0f9ef78c94894ac3e70135d156193b25e23872575d58e2228344964273b4af6b
ghcr.io/berriai/litellm:v1.92.0@sha256:9ef6f45bc0104940571765e610c52a1d761b5ec85efcd193795281086ee61277
```

## Results

| Target | p50 (ms) | p95 (ms) | p99 (ms) | req/s |
|---|---|---|---|---|
| direct | 3.46 | 65.92 | 306.92 | 2740.0 |
| lumen | 3.95 | 11.41 | 98.92 | 6998.9 |
| litellm | 226.96 | 359.84 | 1072.08 | 191.0 |

Added latency = gateway percentile - direct percentile.

## RAM under load (`docker stats --no-stream`, sampled ~10s into each run)

```
CONTAINER   NAME      CPU %     MEM USAGE / LIMIT
58ae6b3a2b98   lumen     140.83%   6.898MiB / 7.818GiB   0.09%     60.6MB / 67.8MB   8.63MB / 0B   10
6a21493020f9   litellm   102.73%   1.021GiB / 7.818GiB   13.06%    1.69MB / 4.1MB   158MB / 56.7MB   17
```

## RAM at idle (post-load, `docker stats --no-stream`)

```
CONTAINER ID   NAME            CPU %     MEM USAGE / LIMIT     MEM %     NET I/O           BLOCK I/O        PIDS
58ae6b3a2b98   lumen           0.00%     5.039MiB / 7.818GiB   0.06%     184MB / 206MB     8.63MB / 0B      10
6a21493020f9   litellm         0.15%     1.033GiB / 7.818GiB   13.21%    5.15MB / 13.2MB   158MB / 59.8MB   17
21b42429f2e8   mock-upstream   104.21%   867.9MiB / 7.818GiB   10.84%    104MB / 152MB     125MB / 115kB    40
```
