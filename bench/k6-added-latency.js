// k6 load script: fixed chat request against $TARGET, reporting p50/p95/p99.
// Compare a gateway target's percentiles against the direct mock baseline; the
// difference is the gateway's added latency.
//
// Besides total request time (http_req_duration), the report also reads time
// to first byte out of k6's built-in http_req_waiting metric (request fully
// written -> first response byte); both land in the --summary-export JSON with
// the trend stats configured below. Streaming time-to-first-chunk cannot be
// measured with stock k6 (it buffers response bodies); that variant lives in
// `cargo bench -p server --bench stream_ttfb`.
import http from "k6/http";
import { check } from "k6";

const TARGET = __ENV.TARGET || "http://localhost:8080";
const KEY = __ENV.OPENAI_API_KEY || "sk-mock";

export const options = {
  scenarios: {
    load: { executor: "constant-vus", vus: 50, duration: "30s" },
  },
  // p99 isn't in k6's default summary trend stats; the head-to-head report
  // (bench/run.sh) needs it alongside the defaults.
  summaryTrendStats: ["avg", "min", "med", "max", "p(90)", "p(95)", "p(99)"],
  thresholds: {
    // Sanity: nothing should be pathologically slow against a 0 ms mock. This
    // is a "did something break" guard, not a strict pass/fail gate - a busy
    // or virtualized host can blow through it without the gateway itself
    // being at fault, so bench/run.sh does not abort the harness on a
    // threshold breach; it still records and reports the real percentiles.
    http_req_duration: ["p(99)<50"],
  },
};

export default function () {
  const res = http.post(
    `${TARGET}/v1/chat/completions`,
    JSON.stringify({
      model: "gpt-4o",
      messages: [{ role: "user", content: "ping" }],
    }),
    { headers: { "Content-Type": "application/json", Authorization: `Bearer ${KEY}` } },
  );
  check(res, { "status is 200": (r) => r.status === 200 });
}
