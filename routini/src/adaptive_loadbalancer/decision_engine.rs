use std::{collections::BTreeSet, sync::Arc, time::Duration};

use crate::{
    adaptive_loadbalancer::options::AdaptiveLbOpt,
    load_balancing::{Backend, Metrics, strategy::Adaptive},
};

pub struct DecisionEngine {
    pub evaluate_strategy_frequency: Duration,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    /// The minimum number of connections needed on the backend with most connections to make use
    /// of the fewest connections strategy.
    pub min_nr_of_connections: usize,
}

impl DecisionEngine {
    pub fn new(opt: &AdaptiveLbOpt) -> Self {
        Self {
            evaluate_strategy_frequency: opt.evaluate_strategy_frequency.clone(),
            connections_divergence_ratio: opt.connections_divergence_ratio.clone(),
            latency_divergence_ratio: opt.latency_divergence_ratio.clone(),
            min_nr_of_connections: opt.min_nr_of_connections.clone(),
        }
    }

    pub async fn evaluate_strategy(
        &self,
        current_strategy: &Adaptive,
        backends: &Arc<BTreeSet<Backend>>,
    ) -> Adaptive {
        match current_strategy {
            Adaptive::FastestServer => {
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                Adaptive::RoundRobin
            }
            Adaptive::FewestConnections => {
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                Adaptive::RoundRobin
            }
            Adaptive::FNVHash => {
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                Adaptive::FNVHash
            }
            Adaptive::RoundRobin => {
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                Adaptive::RoundRobin
            }
            Adaptive::Random => {
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                Adaptive::Random
            }
            Adaptive::Consistent => {
                if self.should_use_fastest_server(backends) {
                    return Adaptive::FastestServer;
                }
                if self.should_use_fewest_connections(backends, self.min_nr_of_connections) {
                    return Adaptive::FewestConnections;
                }
                Adaptive::Consistent
            }
        }
    }

    fn should_use_fastest_server(&self, backends: &Arc<BTreeSet<Backend>>) -> bool {
        if backends.len() < 2 {
            return false;
        }
        let max = backends
            .iter()
            .map(|b| b.metrics.average_latency().unwrap_or(0.0))
            .fold(f32::MIN, f32::max);

        let min = backends
            .iter()
            .map(|b| b.metrics.average_latency().unwrap_or(0.0))
            .fold(f32::MAX, f32::min);

        let ratio = if min > 0.0 { max / min } else { 1.0 };

        if ratio > self.latency_divergence_ratio {
            return true;
        }

        false
    }

    fn should_use_fewest_connections(
        &self,
        backends: &Arc<BTreeSet<Backend>>,
        min_connections: usize,
    ) -> bool {
        if backends.len() < 2 {
            return false;
        }
        // If this is None, connections are not being tracked and fewest connections should never be used
        let Some(max) = backends
            .iter()
            .filter_map(|b| b.metrics.active_connections())
            .reduce(usize::max)
        else {
            return false;
        };

        let Some(min) = backends
            .iter()
            .filter_map(|b| b.metrics.active_connections())
            .reduce(usize::min)
        else {
            return false;
        };

        let ratio = if max >= min_connections {
            max as f32 / min as f32
        } else {
            1.0
        };

        if ratio > self.connections_divergence_ratio {
            return true;
        }
        false
    }
}
