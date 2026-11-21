# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Routini is a high-performance HTTP reverse proxy and load balancer built on Cloudflare's Pingora framework. Its key feature is an **adaptive load balancing system** that automatically switches between different load balancing strategies based on real-time backend metrics (latency and connection counts). Around it sits an nginx-style feature set: virtual hosts, regex locations, redirect/return actions, header rules, timeouts, retries and passive health checks, rate/connection limits, access control, caching, compression, TLS termination and upstream TLS.



## Build, Test, and Run Commands

### Building
```bash
# Build main proxy
cd routini && cargo build

# Release build (optimized)
cd routini && cargo build --release

# Build with profiling symbols
cd routini && cargo build --profile profiling

# Build worker backend server
cd worker && cargo build

# Build plain example
cd plain && cargo build
```

### Running
```bash
# Run main proxy (reads routini/config.json, listens on 0.0.0.0:3500)
cd routini && cargo run

# Other config file (JSON or TOML) / validate only / reload a running proxy in place
cargo run -- --config config.example.toml
cargo run -- --check
kill -HUP <pid>          # or: just reload (docker compose)

# Run worker backend servers (for testing)
cd worker && cargo run

# Run plain example
cd plain && cargo run
```

### Testing
```bash
# Run all tests
cargo nextest run

# Run specific module tests
cargo nextest run --lib load_balancing
cargo nextest run --test load_balancer
cargo nextest run --test proxy

# Run with output
cargo nextest run --no-capture

# Run single test
cargo nextest run <test-name>
```

### Benchmarking
```bash
# Run all benchmarks (generates HTML reports in target/criterion/)
cargo bench

# Run specific benchmark
cargo bench --bench adaptive_load_balancer
cargo bench --bench load_balancer
```

### Logging Configuration
```bash
# Debug level logging
RUST_LOG=debug cargo run

# Module-specific logging
RUST_LOG=routini=debug,pingora=info cargo run

# File logging with daily rotation
LOG_DIR=/tmp/routini-logs cargo run

# JSON format (for log aggregation)
LOG_JSON=1 cargo run

# See routini/examples/logging_demo.sh for complete examples
```

## Workspace Structure

- **routini/** - Main proxy and load balancing library/binary
- **worker/** - Simple Axum-based backend server for testing
- **plain/** - Simplified example using raw Pingora APIs

## Architecture

### Core Components

**Configuration** (`routini/src/config.rs`, `routini/src/utils/config_loader.rs`)
- JSON or TOML: format by extension, else sniffed (leading `{` = JSON); same serde types
- Lookup: `--config`, `$ROUTINI_CONFIG`, then `./config.toml` or `./config.json` (both = error)
- `deny_unknown_fields` everywhere; `Config::validate` reports all problems; `routini --check`
- `Config::into_server_builder` wires every server/route setting into the `ServerBuilder`
- `config.example.toml` mirrors `config.json` (a test keeps them equal)

**ServerBuilder** (`routini/src/server_builder.rs`)
- Fluent API: routes (path, vhost, regex, weighted backends, `AdaptiveLbOpt`, `RouteConfig`),
  TLS, compression, access log, request id, error pages, upgrade/pid/socket paths
- `build_with_reload_handle` also returns the `RouteManager`

**Routes** (`routini/src/route.rs`)
- `RouteConfig`: per-route nginx-style settings (headers, timeouts, retry, passive health,
  limits, upstream TLS, HSTS, actions, cache, access control)
- `RouteRuntime`: a route's load balancer plus its `RouteState` behind an `ArcSwap`

**Proxy Layer** (`routini/src/proxy.rs`)
- `RoutingTable` (default router, vhost routers, regex routes, per-path lookup caches) behind
  an `ArcSwap`; config reload replaces it whole, so caches never outlive their config
- Filters implement the route features; latency/connection metrics recorded per backend
- `https_redirect`: plain listener answers 308 to the TLS listener's port
- Client IP resolved once per request into `ctx.client_ip` (`real_ip.rs`: trusted proxies,
  header read right to left); every per-client feature reads it, never the socket address

**Load Balancing System** (`routini/src/load_balancing/`)
- `Backend<M: Metrics = NoMetric>`: metrics are a monomorphized type parameter
- Six strategies: RoundRobin, Random, FNVHash, Consistent (Ketama), FewestConnections,
  FastestServer (EWMA). `BackendSelection::iter(&'a self)` returns a borrowing GAT iterator
- Static discovery (shared via `Arc<Static>` so reload can change backends), TCP or HTTP
  health checks (`HealthCheckOpt`; HTTP probes follow the route's `upstream_tls`)

**Adaptive Load Balancer** (`routini/src/adaptive_loadbalancer/`)
- `AdaptiveLoadBalancer<D: DecisionEngine>`; config and engine thresholds behind `ArcSwap`,
  `reconfigure` applies new tuning live and wakes the background task
- **Decision Engine**: connection imbalance first (FewestConnections), then latency divergence
  (FastestServer), else the route's starting strategy (a manual `update_strategy` replaces it).
  A hash strategy (FNVHash/Consistent) as the default is never switched away from (affinity)
- **Hash key**: `load_balancer.hash_key` (`HashKey` in `options.rs`, part of the reloadable
  `AdaptiveLbConfig`); `proxy::hash_key` extracts it per request without allocating
  Hysteresis: exit threshold = 1 + (enter - 1) * hysteresis_exit_factor
- **Background task** per route: discovery, health checks, strategy evaluation, selector rebuild

**Config Reload** (`routini/src/reload.rs`)
- `RouteManager` (one Pingora `BackgroundService`) owns every route and its background task
- SIGHUP / `RouteManager::reload`: routes diffed by (host, regex, path) against the applied
  `RouteEntry`; upstreams swapped via discovery (kept backends keep metrics/health), LB tuning
  and strategy applied live, `RouteState` swapped only if route settings changed, a health
  check added/removed/changed rebuilds, routes added/removed
- `server`, `proxy.listener`, `proxy.tls` and changed cert files are reported as restart-required

**TLS** (`routini/src/tls.rs`)
- Pingora `rustls` feature; `proxy.tls` = separate listener, cert/key (relative to the config
  file), `enable_h2` via ALPN
- Pingora's rustls listener panics on bad files and has no cert callback: `check_cert_and_key`
  validates first, rotation = graceful upgrade

**Process Lifecycle** (`routini/src/shutdown.rs`, `routini/src/main.rs`)
- SIGTERM / first SIGINT: graceful drain (5s + 5s defaults); second signal forces exit (130)
- SIGQUIT + a new process started with `--upgrade`: Pingora socket handover (zero downtime);
  refused unless the upgrade socket is ours in a private directory
- `--daemon` forks in `main` before tracing starts threads (Pingora's own fork would lose them)
- `server.pid_file`/`upgrade_sock` default to `$XDG_RUNTIME_DIR` or `/run/routini`, never `/tmp`
- `.env` loaded first (dotenvy); `LogConfig::from_env` reads the logging variables
- systemd unit: `routini/deploy/routini.service`

### Request Flow
1. Client request → Proxy (matchit router matches path)
2. Proxy → Route lookup → AdaptiveLoadBalancer
3. AdaptiveLoadBalancer → Backend selection using current strategy
4. Proxy connects to backend via `upstream_peer()`
5. Request forwarded, response received
6. Metrics recorded (latency EWMA, connection count)
7. Response returned to client

### Background Tasks Flow
- `RouteManager` spawns one task per route running `AdaptiveLoadBalancer`'s background loop
- Each loop: discovery refresh, health checks, strategy evaluation, selector rebuild; it wakes
  on shutdown and on `reconfigure`

## Important Patterns

**Dependency Injection**
- `AdaptiveLoadBalancer<D>` generic over `DecisionEngine` trait
- Allows custom decision logic without modifying core load balancer
- Example: Implement custom `DecisionEngine` for domain-specific strategy selection

**Strategy Pattern**
- `Strategy` trait abstracts load balancing algorithms
- Each strategy provides `BackendSelector` builder and optional metrics requirements
- Enables runtime strategy switching via `AdaptiveSelector`

**Builder Pattern**
- `ServerBuilder` and `Route` use fluent builder APIs
- Example:
  ```rust
  Route::new("/api/*", backends, Adaptive::RoundRobin)?
      .max_iterations(256)
      .include_health_check(Some(Duration::from_secs(1)))
      .route_config(RouteConfig { strip_path_prefix: true })
  ```

**Arc-Swap Pattern**
- Used in `Backends`, `Health`, `LoadBalancer` for lock-free concurrent reads
- Hot-path data structures use `ArcSwap<T>` for atomic updates without read locks

**Trait-based Abstractions**
- Key traits: `ServiceDiscovery`, `HealthCheck`, `Strategy`, `BackendSelection`, `Metrics`
- Enables extensibility and testability

## Key File Locations

**Core Implementation:**
- `routini/src/main.rs` - CLI (`--config`, `--check`, `--upgrade`, `--daemon`), startup order
- `routini/src/config.rs` / `routini/src/utils/config_loader.rs` - Config types, validation, loading
- `routini/src/server_builder.rs` - Server construction API
- `routini/src/proxy.rs` - Pingora `ProxyHttp` filters and the routing table
- `routini/src/route.rs` - Per-route config and runtime state
- `routini/src/reload.rs` - Route ownership and in-process config reload
- `routini/src/shutdown.rs` - Signal handling (graceful shutdown, upgrade)
- `routini/src/tls.rs` - Certificate checks
- `routini/src/utils/runtime_paths.rs` - Private pid/upgrade-socket paths

**Load Balancing / Adaptive System:**
- `routini/src/load_balancing/` - `Backend<M>`, `LoadBalancer`, strategies, discovery, health checks
- `routini/src/adaptive_loadbalancer/` - Adaptive wrapper, decision engine, background loop, options

**Testing:**
- `routini/tests/load_balancer/` - Strategy and adaptive switching tests
- `routini/tests/proxy/` - Routing, reload and TLS end-to-end tests (real servers and backends)
- `routini/benches/` - Criterion benchmarks
- `worker/src/lib.rs` - Test backend server (`/health` echoes its address, `/headers` the
  forwarding headers)

**Utilities:**
- `routini/src/utils/constants.rs` - All default values and constants
- `routini/src/utils/tracing.rs` - Logging configuration
- `routini/LOGGING.md` - Logging documentation

## Development Notes

**Monitoring:**
- Prometheus metrics: `localhost:9090` (`server.prometheus_address`)
- Force a strategy at runtime: change the route's `load_balancer.strategy` and send SIGHUP

**Performance Profiling:**
- Build with `--profile profiling` for profiling symbols
- jemalloc is used as the allocator for performance

**Testing Workflow:**
1. Start worker backends: `cargo run -p worker`
2. Start proxy: `cargo run`
3. Send requests to `localhost:3500/api/*`
4. Monitor metrics and strategy changes

**Configuration Constants:**
- All default values centralized in `routini/src/utils/constants.rs`
- Includes: EWMA smoothing factor, divergence ratios, evaluation frequency, timeouts, endpoints

**Recent Architecture Changes:**
- Remote feature set (vhosts, regex, actions, limits, cache, compression, auth, upstream TLS,
  `Backend<M>`) integrated with: borrowing selector iterators, graceful shutdown, starting-strategy
  fallback and fixed hysteresis, JSON/TOML config with validation, full SIGHUP reload, graceful
  upgrade/daemon with private runtime paths, TLS certificate validation, strategy endpoint removed
