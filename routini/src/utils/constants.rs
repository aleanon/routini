use std::time::Duration;

pub const PROMETHEUS_ENDPOINT_NAME: &str = "prometheus";
pub const PROMETHEUS_ENDPOINT_ADDRESS: &str = "0.0.0.0:9090";
pub const LISTEN_ADDRESS_ENV_VAR: &str = "SERVER_ADDRESS";
pub const PROMETHEUS_ADDRESS_ENV_VAR: &str = "PROMETHEUS_ADDRESS";

// Process management (daemon mode / graceful upgrade)
/// Used when $XDG_RUNTIME_DIR is unset, matches `RuntimeDirectory=routini` in the systemd unit.
/// Never a shared directory like /tmp, see [crate::utils::runtime_paths].
pub const DEFAULT_RUNTIME_DIR: &str = "/run/routini";
pub const PID_FILE_NAME: &str = "routini.pid";
pub const UPGRADE_SOCK_NAME: &str = "routini_upgrade.sock";

// Shutdown
/// Time between receiving SIGTERM/SIGINT and stopping the runtimes. Listeners stop accepting new
/// connections immediately, in-flight requests get this long to finish. Pingora's own default is
/// 300s, which is longer than most orchestrators wait before sending SIGKILL.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(5);
/// Time the runtimes get to exit after the grace period, before being forcefully dropped.
pub const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// Client connections (nginx defaults). Pingora's own defaults keep idle HTTP/1 connections and
// stalled writes open forever.
/// Idle time before a kept-alive HTTP/1 connection is closed (nginx `keepalive_timeout`).
pub const DEFAULT_KEEPALIVE_TIMEOUT_SECS: u64 = 75;
/// Longest wait for the next piece of a request body (nginx `client_body_timeout`).
pub const DEFAULT_CLIENT_BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest wait for the client to accept more of the response (nginx `send_timeout`).
pub const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(60);

// Router defaults
pub const DEFAULT_PATH_REMAINDER_IDENTIFIER: &str = "rest";
pub const DEFAULT_WILDCARD_IDENTIFIER: &str = "{*rest}";
/// Maximum number of distinct request paths whose routing result (matched backend pool +
/// pre-computed stripped path) is cached. Bounded to avoid unbounded growth from path
/// parameters (e.g. `/api/users/{id}`). Evicted entries simply pay the matchit lookup +
/// stripping cost again on their next request.
pub const DEFAULT_PATH_CACHE_CAPACITY: usize = 8192;

// Load balancer defaults
pub const DEFAULT_MAX_ALGORITHM_ITERATIONS: usize = 256;
pub const DEFAULT_SMOOTHING_FACTOR: f32 = 0.5;
pub const DEFAULT_CONNECTIONS_DIV_RATIO: f32 = 1.2;
pub const DEFAULT_LATENCY_DIV_RATIO: f32 = 2.0;
pub const DEFAULT_EVALUATE_STRATEGY_FREQUENCY: Duration = Duration::from_secs(5);
pub const DEFAULT_HEALTH_CHECK_FREQUENCY: Duration = Duration::from_secs(1);
/// How often FewestConnections and FastestServer re-sort their backends by the live metrics.
pub const DEFAULT_SELECTOR_REBUILD_INTERVAL: Duration = Duration::from_millis(200);
/// How often upstream DNS names are resolved again. Upstreams given as IP addresses don't
/// need it, discovery is then a cheap no-op compare.
pub const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_MIN_NR_OF_CONNECTIONS: usize = 1000;
/// Fraction of an enter threshold used as the exit threshold, giving the adaptive
/// engine hysteresis so it does not flap between strategies on every evaluation.
/// e.g. with a latency enter ratio of 2.0 and factor 0.75, the engine only leaves
/// FastestServer once the ratio drops below 1.5.
pub const DEFAULT_HYSTERESIS_EXIT_FACTOR: f32 = 0.75;

// Logging
pub const DEFAULT_LOG_LEVEL_FILTER: &str = "info,routini=debug,pingora=info";
pub const DEFAULT_LOG_JSON: bool = false;
pub const DEFAULT_MAX_LOG_AGE_DAYS: u64 = 7;
