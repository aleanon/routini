use std::time::Duration;

pub const PROMETHEUS_ENDPOINT_NAME: &str = "prometheus";
pub const PROMETHEUS_ENDPOINT_ADDRESS: &str = "0.0.0.0:9090";
pub const LISTEN_ADDRESS_ENV_VAR: &str = "SERVER_ADDRESS";
pub const PROMETHEUS_ADDRESS_ENV_VAR: &str = "PROMETHEUS_ADDRESS";

// Shutdown
/// Time between receiving SIGTERM/SIGINT and stopping the runtimes. Listeners stop accepting new
/// connections immediately, in-flight requests get this long to finish. Pingora's own default is
/// 300s, which is longer than most orchestrators wait before sending SIGKILL.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(5);
/// Time the runtimes get to exit after the grace period, before being forcefully dropped.
pub const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
