use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use pingora::protocols::l4::socket::SocketAddr;
use tokio::sync::RwLock;

use crate::{
    adaptive_loadbalancer::backend_metrics::BackendMetrics, load_balancing::strategy::Adaptive,
};

pub struct DecisionEngine {
    pub metrics: RwLock<HashMap<SocketAddr, Arc<BackendMetrics>>>,
    pub evaluate_strategy_frequency: Duration,
    pub connections_divergence_ratio: f64,
    pub latency_divergence_ratio: f64,
}

impl DecisionEngine {
    pub fn new(
        metrics: HashMap<SocketAddr, Arc<BackendMetrics>>,
        evaluate_strategy_frequency: Duration,
        connections_divergence_ratio: f64,
        latency_divergence_ratio: f64,
    ) -> Self {
        Self {
            metrics: RwLock::new(metrics),
            evaluate_strategy_frequency,
            connections_divergence_ratio,
            latency_divergence_ratio,
        }
    }

    pub async fn record_latency(&self, addr: SocketAddr, latency: u32) {
        let metrics = self.metrics.read().await;
        if let Some(metric) = metrics.get(&addr) {
            metric.record_latency(latency);
        }
    }

    pub async fn increment_connection_count(&self, addr: SocketAddr) {
        let metrics = self.metrics.read().await;
        if let Some(metric) = metrics.get(&addr) {
            metric.increment_connection_count();
        }
    }

    pub async fn decrement_connection_count(&self, addr: SocketAddr) {
        let metrics = self.metrics.read().await;
        if let Some(metric) = metrics.get(&addr) {
            metric.decrement_connection_count();
        }
    }

    pub async fn evaluate_strategy(&self, _current_strategy: &Adaptive) -> Adaptive {
        if self.should_use_fastest_server().await {
            //TODO: Implement fastest server
            return Adaptive::FNVHash;
        }

        if self.should_use_fewest_connections().await {
            return Adaptive::FewestConnections;
        }

        Adaptive::FNVHash
    }

    async fn should_use_fastest_server(&self) -> bool {
        let latencies: Vec<f64> = self
            .metrics
            .read()
            .await
            .values()
            .map(|b| b.avg_latency.load(Ordering::Relaxed))
            .collect();

        if latencies.len() >= 2 {
            let max = latencies.iter().cloned().fold(f64::MIN, f64::max);
            let min = latencies.iter().cloned().fold(f64::MAX, f64::min);
            let ratio = if min > 0.0 { max / min } else { 1.0 };

            if ratio > self.latency_divergence_ratio {
                return true;
            }
        }
        false
    }

    async fn should_use_fewest_connections(&self) -> bool {
        let connections: Vec<usize> = self
            .metrics
            .read()
            .await
            .values()
            .map(|b| b.connection_count.load(Ordering::Relaxed))
            .collect();

        if connections.len() >= 2 {
            let max = connections.iter().cloned().max().unwrap_or(0);
            let min = connections.iter().cloned().min().unwrap_or(0);

            let ratio = if min > 0 {
                max as f64 / min as f64
            } else {
                1.0
            };

            if ratio > self.connections_divergence_ratio {
                return true;
            }
        }
        false
    }
}
