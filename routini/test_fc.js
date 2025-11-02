// Testing automatic switching to fewest connections, using a keep alive endpoint to
// get uneven connections distribution

import http from "k6/http";
import { check, sleep } from "k6";

export let options = {
  vus: 50, // 10 concurrent virtual users
  iterations: 50, // total number of requests (distributed across VUs)
};

// Base URL of your load balancer
const BASE_URL = "http://localhost:3500/api";

export default function () {
  // Each virtual user has a unique ID starting from 1
  const userId = __VU;

  // Every 3rd user targets the alternate endpoint
  const endpoint = userId % 3 === 0 ? "/keepalive" : "/health";

  const res = http.get(`${BASE_URL}${endpoint}`);

  check(res, { "status is 200": (r) => r.status === 200 });

  // Optional short sleep to simulate realistic pacing
  sleep(0.1);
}
