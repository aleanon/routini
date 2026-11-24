# routini

An HTTP reverse proxy and load balancer built on Cloudflare's
[Pingora](https://github.com/cloudflare/pingora). Each route picks its load-balancing strategy
from what its backends are doing: when connection counts drift apart it sends requests to the
least busy backend, when latencies drift apart to the fastest one, and otherwise it uses the
strategy you configured. Around that sits an nginx-style feature set, configured in TOML or
JSON and reloadable without a restart.

## Features

- **Routing**: path prefixes and regex locations, virtual hosts (exact names and
  `*.example.com` wildcards, nginx `server_name` semantics), prefix stripping, redirect and
  fixed-response routes
- **Load balancing**: round robin, random, FNV hash, consistent hashing (Ketama), fewest
  connections and fastest server (latency EWMA), weighted backends, hash keys from the client
  address, URI, path, a header or a cookie. Upstreams may be hostnames, re-resolved on an interval
- **Adaptive strategy switching** per route, with hysteresis so it doesn't flap (see below)
- **Health**: active TCP or HTTP health checks, passive health (failures take a backend out),
  retries on another backend for connect errors, timeouts and chosen statuses
- **Traffic control**: per-client rate limits (with or without delaying the burst), per-client
  connection limits, per-listener connection and TLS handshake limits, maximum body size,
  client and backend timeouts
- **Access control**: IP allow and deny lists, HTTP Basic auth (bcrypt), trusted proxies for
  the real client address
- **HTTP**: request and response header rules, `X-Forwarded-*`, request IDs, HSTS, response
  caching, compression, custom error pages, access log
- **TLS**: termination with SNI certificate selection, TLS 1.2 or 1.3 minimum, HTTP/2, client
  certificates (mTLS), handshakes on dedicated threads; HTTPS to backends with a custom CA,
  client certificates and optional verification
- **Operations**: config reload on SIGHUP, graceful shutdown, zero-downtime binary upgrades
  (socket handover), daemon mode, a systemd unit, Prometheus metrics

## Quick start

Requires a recent stable Rust toolchain (1.88 or newer).

```bash
# Two test backends (they answer /health with the address they listen on)
cd worker
PORT=4001 cargo run &
PORT=4002 cargo run &

# The proxy, on port 3500, with a route for /api/* over those backends
cd ../routini
cat > quickstart.toml <<'EOF'
[proxy]
listener = 3500

[[proxy.router]]
path = "/api/*"

[proxy.router.load_balancer]
strategy = "RoundRobin"

[[proxy.router.load_balancer.upstreams]]
address = "127.0.0.1:4001"

[[proxy.router.load_balancer.upstreams]]
address = "127.0.0.1:4002"
EOF
cargo run --release -- --config quickstart.toml

curl localhost:3500/api/health   # 0.0.0.0:4001, then 0.0.0.0:4002, ...
```

`compose.yml` runs the same setup in Docker with 40 backends: `just compose` builds and starts
it, `just reload` reloads the proxy's config.

## Configuration

routini reads one config file, TOML or JSON (same keys, the format follows the extension):
`--config <path>`, else `$ROUTINI_CONFIG`, else `config.toml` or `config.json` in the working
directory. [`routini/config.example.toml`](routini/config.example.toml) documents every key
with its default and its nginx equivalent.

Unknown keys are rejected, and every problem is reported at once rather than only the first.
Check a file without starting the proxy:

```bash
routini --check --config config.toml
```

`SERVER_ADDRESS` and `PROMETHEUS_ADDRESS` override the listen and metrics addresses. A `.env`
file in the working directory is loaded first.

## Adaptive load balancing

Every route runs a background task that evaluates its backends every
`evaluate_strategy_frequency_secs` (default 5) and picks a strategy, in this order:

1. **Fewest connections**, when the busiest backend has at least `min_nr_of_connections`
   (1000) open and more than `connections_divergence_ratio` (1.2) times the connections of the
   least busy one.
2. **Fastest server**, when the slowest backend's latency (an EWMA,
   `latency_smoothing_factor` 0.5) is more than `latency_divergence_ratio` (2.0) times the
   fastest one's.
3. Otherwise the route's configured `strategy`.

To avoid switching back and forth, leaving a strategy needs the ratio to drop below a lower
threshold: `1 + (enter - 1) * hysteresis_exit_factor` (0.75). A hash strategy (`FNVHash`,
`Consistent`) is never switched away from, since clients rely on reaching the same backend.

The tuning lives under `[proxy.router.load_balancer.adaptive_lb_opt]` and is applied live on
reload. Changing a route's `strategy` and reloading also forces a strategy at runtime.

## Operations

| Action | How | Effect |
|---|---|---|
| Reload config | `kill -HUP <pid>` | Routes, backends, load-balancing tuning, route settings and certificates change in place; backends that stay keep their metrics and health. `[server]`, the listeners and the TLS listener settings need a restart, and the reload says so |
| Graceful shutdown | SIGTERM or SIGINT | Stops accepting, drains in-flight requests (5s grace + 5s timeout by default); a second signal exits at once |
| Zero-downtime upgrade | start `routini --upgrade`, then SIGQUIT the old process | The new process takes over the listening sockets, the old one drains and exits |
| Run in the background | `routini --daemon` | Writes `server.pid_file` |

The pid file and upgrade socket default to `$XDG_RUNTIME_DIR` or `/run/routini`, and an
upgrade is refused unless the socket is in a directory private to the user.
[`routini/deploy/routini.service`](routini/deploy/routini.service) is a systemd unit where
`systemctl reload` does the upgrade and `systemctl kill -s HUP` the in-place reload.

## Monitoring

Prometheus metrics are served on `server.prometheus_address` (default `0.0.0.0:9090`):
Pingora's own, plus

- `routini_requests_total`, `routini_request_duration_seconds`: per route and status
- `routini_upstream_response_seconds`, `routini_upstream_connect_seconds` (TCP and TLS),
  `routini_upstream_failures_total`: per backend
- `routini_backend_healthy`, `routini_backend_active_connections`,
  `routini_backend_latency_ewma_seconds`, `routini_route_strategy`: read from the live routing
  table at scrape time
- `routini_listener_open_connections`, `routini_listener_pending_handshakes`,
  `routini_listener_closed_total`: per listener, with the reason a connection was closed

## Logging

Logs go to stdout, `RUST_LOG` sets the level (`RUST_LOG=routini=debug,pingora=info`),
`LOG_JSON=1` switches to JSON and `LOG_DIR` adds daily-rotated files. See
[`routini/LOGGING.md`](routini/LOGGING.md).

## Development

| Directory | Contents |
|---|---|
| `routini/` | The proxy: library and binary |
| `worker/` | A small Axum backend for tests and local runs |
| `plain/` | The same kind of proxy written directly against Pingora's APIs, for comparison |

The three are separate Cargo packages. In `routini/`:

```bash
cargo nextest run          # the tests (nextest is required: tests need a process each)
cargo bench                # Criterion benchmarks, reports in target/criterion/
cargo build --profile profiling   # release build with symbols, see PROFILING.md
```

CI runs `cargo fmt --check`, clippy with warnings denied and the tests for each package, and
builds the Docker images.

routini depends on a [fork of Pingora](https://github.com/aleanon/pingora/tree/rustls-server-config)
that adds two things on top of upstream `main`: a rustls listener that takes a caller-built
`ServerConfig` (for the minimum TLS version, mTLS and certificates swapped on reload), and a
hook at the end of a proxied stream.

## License

Apache-2.0, see [`routini/LICENSE`](routini/LICENSE).
