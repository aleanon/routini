import http from "k6/http";
import { check } from "k6";

export let options = {
  stages: [
    { duration: "25s", target: 100 }, // warm-up
    { duration: "25s", target: 500 }, //light load
    { duration: "25s", target: 1000 }, // medium load
    { duration: "25s", target: 1500 }, // high load
    { duration: "25s", target: 2000 }, // heavy load
    { duration: "25s", target: 0 }, // cool down
  ],
  thresholds: {
    // You can tune these thresholds to define "acceptable" performance
    http_req_failed: ["rate<0.01"], // <1% errors
    http_req_duration: ["p(95)<200"], // 95% of requests under 200ms
  },
};

const BASE_URL = "http://localhost:3500/api";
// const BASE_URL = "http://localhost:4001";

export default function () {
  let responses = http.batch([
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
    ["GET", `${BASE_URL}/health`],
  ]);

  for (const res of responses) {
    check(res, {
      "status is 200": (r) => r.status === 200,
    });
  }
}
