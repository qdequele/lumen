// k6 load script: fixed chat request against $TARGET, reporting p50/p95/p99.
// Compare a gateway target's percentiles against the direct mock baseline; the
// difference is the gateway's added latency.
import http from "k6/http";
import { check } from "k6";

const TARGET = __ENV.TARGET || "http://localhost:8080";
const KEY = __ENV.OPENAI_API_KEY || "sk-mock";

export const options = {
  scenarios: {
    load: { executor: "constant-vus", vus: 50, duration: "30s" },
  },
  thresholds: {
    // Sanity: nothing should be pathologically slow against a 0 ms mock.
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
