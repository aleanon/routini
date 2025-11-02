import http from "k6/http";
import { sleep } from "k6";

export let options = {
  vus: 1000,
  duration: "10s",
};

export default function () {
  http.batch([
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
    ["GET", "http://localhost:3500/api/health"],
  ]);
  sleep(1);
}
