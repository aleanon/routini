use std::time::Duration;

use crate::{
    load_balancing::strategy::Adaptive,
    utils::constants::{
        DEFAULT_CONNECTIONS_DIV_RATIO, DEFAULT_LATENCY_DIV_RATIO, DEFAULT_MAX_ALGORITHM_ITERATIONS,
        DEFAULT_SMOOTHING_FACTOR,
    },
};

pub struct AdaptiveLbOpt {
    pub latency_ewma_smoothing_factor: f64,
    pub connections_divergence_ratio: f64,
    pub latency_divergence_ratio: f64,
    pub starting_strategy: Adaptive,
    pub evaluate_strategy_frequency: Duration,
    pub max_iterations: usize,
    pub health_check_interval: Option<Duration>,
}

impl Default for AdaptiveLbOpt {
    fn default() -> Self {
        Self {
            latency_ewma_smoothing_factor: DEFAULT_SMOOTHING_FACTOR,
            connections_divergence_ratio: DEFAULT_CONNECTIONS_DIV_RATIO,
            latency_divergence_ratio: DEFAULT_LATENCY_DIV_RATIO,
            starting_strategy: Adaptive::default(),
            evaluate_strategy_frequency: Duration::from_millis(1000),
            max_iterations: DEFAULT_MAX_ALGORITHM_ITERATIONS,
            health_check_interval: Some(Duration::from_secs(1)),
        }
    }
}
