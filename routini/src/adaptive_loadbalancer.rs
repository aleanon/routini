pub mod background_service;
pub mod decision_engine;
pub mod options;

use std::{collections::BTreeSet, sync::Arc};

use crate::{
    adaptive_loadbalancer::{
        decision_engine::DecisionEngine,
        options::{AdaptiveLbConfig, AdaptiveLbOpt},
    },
    load_balancing::{
        Backend, Backends, LoadBalancer,
        health_check::TcpHealthCheck,
        strategy::{Adaptive, adaptive::AdaptiveStrategyMetrics},
    },
};

/// The metrics type the adaptive load balancer pins its backends to.
pub type AdaptiveBackend = Backend<AdaptiveStrategyMetrics>;
/// A [`Backends`] collection pinned to the adaptive metrics type.
pub type AdaptiveBackends = Backends<AdaptiveStrategyMetrics>;

pub struct AdaptiveLoadBalancer<D> {
    lb: LoadBalancer<Adaptive, AdaptiveStrategyMetrics>,
    decision_engine: D,
    pub config: AdaptiveLbConfig,
}

impl<D: DecisionEngine> AdaptiveLoadBalancer<D> {
    pub fn from_backends(
        backends: AdaptiveBackends,
        options: Option<AdaptiveLbOpt>,
        decision_engine: D,
    ) -> Self {
        let options = options.unwrap_or_default();
        let mut lb =
            LoadBalancer::from_backends_with_strategy(backends, options.starting_strategy.clone());

        if options.health_check_interval.is_some() {
            let hc = TcpHealthCheck::new();
            lb.set_health_check(hc);
            lb.health_check_frequency = options.health_check_interval.clone()
        }

        Self {
            lb,
            decision_engine,
            config: AdaptiveLbConfig::from(options),
        }
    }

    pub fn backends(&self) -> Arc<BTreeSet<AdaptiveBackend>> {
        self.lb.backends().get_backend()
    }

    pub fn select(&self, key: &[u8]) -> Option<AdaptiveBackend> {
        self.lb.select(key, self.config.max_iterations)
    }

    /// Select a healthy backend whose address is not in `exclude`. Used for per-request failover
    /// so a retry lands on a different backend than the one that just failed.
    pub fn select_excluding(
        &self,
        exclude: &[pingora::protocols::l4::socket::SocketAddr],
    ) -> Option<AdaptiveBackend> {
        self.lb
            .select_with(&[], self.config.max_iterations, |backend, healthy| {
                healthy && !exclude.iter().any(|addr| addr == &backend.addr)
            })
    }

    /// Manually enable/disable a backend (used by passive health checks to eject/restore).
    pub fn set_backend_enabled(&self, backend: &AdaptiveBackend, enabled: bool) {
        self.lb.backends().set_enable(backend, enabled);
    }

    /// The strategy the load balancer is currently selecting backends with.
    pub async fn current_strategy(&self) -> Adaptive {
        self.lb.current_strategy().await
    }

    pub fn decision_engine(&self) -> &D {
        &self.decision_engine
    }

    /// Manually override the strategy. It also becomes the decision engine's default, so the
    /// engine returns to it (instead of the configured starting strategy) whenever the backends
    /// are balanced. Returns false if the strategy was already in use.
    pub async fn update_strategy(&self, new_strategy: Adaptive) -> bool
    where
        D: DecisionEngine<Strategy = Adaptive>,
    {
        self.decision_engine
            .set_default_strategy(new_strategy.clone());
        self.lb.update_strategy(new_strategy).await
    }
}
