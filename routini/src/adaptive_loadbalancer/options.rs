use std::time::Duration;

use crate::{
    load_balancing::strategy::Adaptive,
    utils::constants::{
        DEFAULT_CONNECTIONS_DIV_RATIO, DEFAULT_EVALUATE_STRATEGY_FREQUENCY,
        DEFAULT_HEALTH_CHECK_FREQUENCY, DEFAULT_LATENCY_DIV_RATIO,
        DEFAULT_MAX_ALGORITHM_ITERATIONS, DEFAULT_MIN_NR_OF_CONNECTIONS, DEFAULT_SMOOTHING_FACTOR,
    },
};

/// The Options to pass inn during construction of the load balancer.
pub struct AdaptiveLbOpt {
    pub latency_smoothing_factor: f32,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    pub starting_strategy: Adaptive,
    pub evaluate_strategy_frequency: Duration,
    pub max_iterations: usize,
    pub health_check_interval: Option<Duration>,
    pub min_nr_of_connections: usize,
}

impl Default for AdaptiveLbOpt {
    fn default() -> Self {
        Self {
            latency_smoothing_factor: DEFAULT_SMOOTHING_FACTOR,
            connections_divergence_ratio: DEFAULT_CONNECTIONS_DIV_RATIO,
            latency_divergence_ratio: DEFAULT_LATENCY_DIV_RATIO,
            starting_strategy: Adaptive::default(),
            evaluate_strategy_frequency: DEFAULT_EVALUATE_STRATEGY_FREQUENCY,
            max_iterations: DEFAULT_MAX_ALGORITHM_ITERATIONS,
            health_check_interval: Some(DEFAULT_HEALTH_CHECK_FREQUENCY),
            min_nr_of_connections: DEFAULT_MIN_NR_OF_CONNECTIONS,
        }
    }
}

/// The stored configuration to be used at runtime.
pub struct AdaptiveLbConfig {
    pub latency_smoothing_factor: f32,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    pub evaluate_strategy_frequency: Duration,
    pub max_iterations: usize,
    pub health_check_interval: Option<Duration>,
    pub min_nr_of_connections: usize,
}

impl From<AdaptiveLbOpt> for AdaptiveLbConfig {
    fn from(value: AdaptiveLbOpt) -> Self {
        Self {
            latency_smoothing_factor: value.latency_smoothing_factor,
            connections_divergence_ratio: value.connections_divergence_ratio,
            latency_divergence_ratio: value.latency_divergence_ratio,
            evaluate_strategy_frequency: value.evaluate_strategy_frequency,
            max_iterations: value.max_iterations,
            health_check_interval: value.health_check_interval,
            min_nr_of_connections: value.min_nr_of_connections,
        }
    }
}
