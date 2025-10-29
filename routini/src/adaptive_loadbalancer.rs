pub mod backend_metrics;
pub mod background_service;
pub mod decision_engine;
pub mod options;

use std::{sync::Arc, time::Duration};

use color_eyre::eyre::Result;
use pingora::protocols::l4::socket::SocketAddr;

use crate::{
    adaptive_loadbalancer::{
        backend_metrics::BackendMetrics, decision_engine::DecisionEngine, options::AdaptiveLbOpt,
    },
    load_balancing::{Backend, LoadBalancer, health_check::TcpHealthCheck, strategy::Adaptive},
};
pub struct AdaptiveLoadBalancer {
    lb: LoadBalancer<Adaptive>,
    decision_engine: DecisionEngine,
    max_iterations: usize,
}

impl AdaptiveLoadBalancer {
    pub fn try_from_iter(
        iter: impl IntoIterator<Item = String>,
        options: Option<AdaptiveLbOpt>,
    ) -> Result<Self> {
        let options = options.unwrap_or_default();
        let mut lb = LoadBalancer::try_from_iter_with_strategy(iter, options.starting_strategy)?;
        let metrics = lb
            .backends()
            .get_backend()
            .iter()
            .map(|b| {
                (
                    b.addr.clone(),
                    Arc::new(BackendMetrics::new(options.latency_ewma_smoothing_factor)),
                )
            })
            .collect();

        if options.health_check_interval.is_some() {
            let hc = TcpHealthCheck::new();
            lb.set_health_check(hc);
            lb.health_check_frequency = options.health_check_interval
        }

        let decision_engine = DecisionEngine::new(
            metrics,
            options.evaluate_strategy_frequency,
            options.connections_divergence_ratio,
            options.latency_divergence_ratio,
        );

        Ok(Self {
            lb,
            decision_engine,
            max_iterations: options.max_iterations,
        })
    }

    pub fn select(&self, key: &[u8]) -> Option<Backend> {
        self.lb.select(key, self.max_iterations)
    }

    pub async fn update_strategy(&self, new_strategy: Adaptive) {
        self.lb.update_strategy(new_strategy).await;
    }

    pub async fn record_latency(&self, addr: SocketAddr, latency: Duration) {
        self.decision_engine
            .record_latency(addr, latency.subsec_millis())
            .await
    }

    pub async fn increment_connections(&self, addr: SocketAddr) {
        self.decision_engine.increment_connection_count(addr).await
    }

    pub async fn decrement_connections(&self, addr: SocketAddr) {
        self.decision_engine.decrement_connection_count(addr).await
    }
}
