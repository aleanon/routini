use std::time::Duration;

pub const SET_STRATEGY_ENDPOINT_NAME: &str = "strategy_updater";
pub const SET_STRATEGY_ENDPOINT_ADDRESS: &str = "0.0.0.0:5000";
pub const PROMETHEUS_ENDPOINT_NAME: &str = "prometheus";
pub const PROMETHEUS_ENDPOINT_ADDRESS: &str = "0.0.0.0:9090";

pub const PATH_REMAINDER_IDENTIFIER: &str = "rest";
pub const WILDCARD_IDENTIFIER: &str = "{*rest}";

pub const DEFAULT_MAX_ALGORITHM_ITERATIONS: usize = 256;
pub const DEFAULT_SMOOTHING_FACTOR: f32 = 0.5;
pub const DEFAULT_CONNECTIONS_DIV_RATIO: f32 = 1.2;
pub const DEFAULT_LATENCY_DIV_RATIO: f32 = 2.0;
pub const DEFAULT_EVALUATE_STRATEGY_FREQUENCY: Duration = Duration::from_secs(5);
pub const DEFAULT_HEALTH_CHECK_FREQUENCY: Duration = Duration::from_secs(1);
pub const DEFAULT_MIN_NR_OF_CONNECTIONS: usize = 1000;
