#!/usr/bin/env bash
# Drives the full LUMEN-vs-LiteLLM head-to-head (M7 §7.1, issue #27) end to
# end and writes a timestamped, reproducible report to bench/results/.
#
# Usage: bench/run.sh
#
# Requires: docker (with compose v2), k6, jq. Pulls/builds three containers
# (mock upstream, LUMEN, LiteLLM - versions pinned in compose.yaml) and runs
# the k6 load script against each in turn, then tears everything down.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/compose.yaml"
RESULTS_DIR="$SCRIPT_DIR/results"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="$RESULTS_DIR/$STAMP"

for bin in docker k6 jq; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "error: '$bin' is required but not on PATH" >&2
    exit 1
  fi
done

mkdir -p "$RUN_DIR"
cd "$REPO_ROOT"

cleanup() {
  docker compose -f "$COMPOSE_FILE" down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> Building and starting mock-upstream, lumen, litellm"
docker compose -f "$COMPOSE_FILE" up -d --build

wait_for() {
  local url="$1" name="$2" method="${3:-GET}" tries=60
  until curl -fsS -X "$method" -o /dev/null "$url" 2>/dev/null; do
    tries=$((tries - 1))
    if [ "$tries" -le 0 ]; then
      echo "error: $name never became ready at $url" >&2
      docker compose -f "$COMPOSE_FILE" logs --tail=50 >&2 || true
      exit 1
    fi
    sleep 2
  done
}

echo "==> Waiting for services to become ready"
# mockserver's status endpoint only answers PUT, not GET.
wait_for "http://localhost:1080/mockserver/status" "mock-upstream" PUT
wait_for "http://localhost:8080/health" "lumen"
# LiteLLM's /health/liveliness is unauthenticated in v1.92.0.
wait_for "http://localhost:4000/health/liveliness" "litellm"

run_k6() {
  local name="$1" target="$2" container="${3:-}"
  echo "==> Running k6 against $name ($target)"
  # For the gateway targets, sample `docker stats` for that specific
  # container partway through the run so RAM is measured *under load*, per
  # bench/README.md's stated methodology, not at idle after the fact.
  local stats_pid=""
  if [ -n "$container" ]; then
    (
      sleep 10
      docker stats --no-stream "$container" > "$RUN_DIR/$container.stats-under-load.txt" 2>&1
    ) &
    stats_pid=$!
  fi
  # k6 exits non-zero when a threshold is crossed (see k6-added-latency.js:
  # the p99<50ms threshold is a sanity guard, not a pass/fail gate for this
  # harness) - don't let `set -e`/pipefail abort the whole run over that; the
  # summary JSON and percentiles are what this script actually needs.
  set +e
  OPENAI_API_KEY=sk-mock TARGET="$target" k6 run \
    --summary-export="$RUN_DIR/$name.summary.json" \
    "$SCRIPT_DIR/k6-added-latency.js" \
    | tee "$RUN_DIR/$name.log"
  local k6_status="${PIPESTATUS[0]}"
  set -e
  if [ -n "$stats_pid" ]; then
    wait "$stats_pid" || true
  fi
  if [ "$k6_status" -ne 0 ]; then
    echo "warning: k6 against $name exited $k6_status (likely the sanity threshold; see $RUN_DIR/$name.log)"
  fi
}

# Direct-to-mock first (baseline), then each gateway. Run one at a time so
# they don't contend for CPU on the host.
run_k6 direct "http://localhost:1080"
run_k6 lumen "http://localhost:8080" lumen
run_k6 litellm "http://localhost:4000" litellm

echo "==> Capturing idle RAM (post-load) via docker stats"
docker stats --no-stream lumen litellm mock-upstream \
  > "$RUN_DIR/docker-stats-idle.txt"

echo "==> Recording pinned third-party image references"
# Both are pinned by tag + digest directly in compose.yaml; echo them back
# rather than re-deriving via `docker inspect` (RepoDigests is an image-level
# field, not a container one, and is empty for images resolved by digest on
# some daemons). lumen itself is built from this checkout's Dockerfile, so
# the git commit recorded below is its provenance.
grep -E '^[[:space:]]*image:' "$COMPOSE_FILE" \
  | sed -E 's/^[[:space:]]*image:[[:space:]]*//' \
  > "$RUN_DIR/image-digests.txt"

# --- Build the Markdown report ---------------------------------------------

pct() { # pct <file> <metric> <field> - 2-decimal ms
  jq -r --arg m "$2" --arg f "$3" '.metrics[$m][$f] // empty' "$1" \
    | awk '{printf "%.2f\n", $1}' \
    | grep . || echo "n/a"
}

rate() { # rate <file> - 1-decimal req/s
  jq -r '.metrics.http_reqs.rate // empty' "$1" \
    | awk '{printf "%.1f\n", $1}' \
    | grep . || echo "n/a"
}

REPORT="$RUN_DIR/report.md"
{
  echo "# LUMEN vs LiteLLM benchmark run - $STAMP"
  echo
  echo "Reproduced with \`bench/run.sh\` (see \`bench/README.md\` for methodology"
  echo "and \`docs/perf-baseline.md\` for how this fits the performance pillar)."
  echo
  echo "## Environment"
  echo
  echo "| | |"
  echo "|---|---|"
  echo "| Host | $(uname -s) $(uname -m) |"
  echo "| Docker | $(docker version --format '{{.Server.Version}}') |"
  echo "| k6 | $(k6 version | head -n1) |"
  echo "| Git commit | $(git -C "$REPO_ROOT" rev-parse --short HEAD) |"
  echo
  echo "Pinned images actually run (tag + resolved digest):"
  echo
  echo '```'
  cat "$RUN_DIR/image-digests.txt"
  echo '```'
  echo
  echo "## Results"
  echo
  echo "| Target | p50 (ms) | p95 (ms) | p99 (ms) | req/s |"
  echo "|---|---|---|---|---|"
  for name in direct lumen litellm; do
    f="$RUN_DIR/$name.summary.json"
    p50=$(pct "$f" http_req_duration med)
    p95=$(pct "$f" http_req_duration "p(95)")
    p99=$(pct "$f" http_req_duration "p(99)")
    rps=$(rate "$f")
    echo "| $name | $p50 | $p95 | $p99 | $rps |"
  done
  echo
  echo "Added latency = gateway percentile - direct percentile."
  echo
  echo "### Time to first byte"
  echo
  echo "k6's \`http_req_waiting\`: the gap between the request being fully"
  echo "written and the first byte of the response arriving, i.e. how long the"
  echo "target sat on the request before starting to answer. Added TTFB ="
  echo "gateway percentile - direct percentile."
  echo
  echo "| Target | p50 (ms) | p95 (ms) | p99 (ms) |"
  echo "|---|---|---|---|"
  for name in direct lumen litellm; do
    f="$RUN_DIR/$name.summary.json"
    p50=$(pct "$f" http_req_waiting med)
    p95=$(pct "$f" http_req_waiting "p(95)")
    p99=$(pct "$f" http_req_waiting "p(99)")
    echo "| $name | $p50 | $p95 | $p99 |"
  done
  echo
  echo "## RAM under load (\`docker stats --no-stream\`, sampled ~10s into each run)"
  echo
  echo '```'
  echo "CONTAINER   NAME      CPU %     MEM USAGE / LIMIT"
  for c in lumen litellm; do
    f="$RUN_DIR/$c.stats-under-load.txt"
    if [ -f "$f" ]; then
      tail -n +2 "$f"
    fi
  done
  echo '```'
  echo
  echo "## RAM at idle (post-load, \`docker stats --no-stream\`)"
  echo
  echo '```'
  cat "$RUN_DIR/docker-stats-idle.txt"
  echo '```'
} > "$REPORT"

echo "==> Wrote $REPORT"
echo "==> Also see: $RUN_DIR/*.summary.json (raw k6 output) and $RUN_DIR/*.log"
