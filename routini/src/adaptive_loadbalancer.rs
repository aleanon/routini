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
        Backend, Backends, LoadBalancer, health_check::TcpHealthCheck, strategy::Adaptive,
    },
};
pub struct AdaptiveLoadBalancer {
    lb: LoadBalancer<Adaptive>,
    decision_engine: DecisionEngine,
    pub config: AdaptiveLbConfig,
}

impl AdaptiveLoadBalancer {
    pub fn from_backends(backends: Backends, options: Option<AdaptiveLbOpt>) -> Self {
        let options = options.unwrap_or_default();
        let mut lb =
            LoadBalancer::from_backends_with_strategy(backends, options.starting_strategy.clone());

        if options.health_check_interval.is_some() {
            let hc = TcpHealthCheck::new();
            lb.set_health_check(hc);
            lb.health_check_frequency = options.health_check_interval.clone()
        }

        let decision_engine = DecisionEngine::new(&options);

        Self {
            lb,
            decision_engine,
            config: AdaptiveLbConfig::from(options),
        }
    }

    pub fn backends(&self) -> Arc<BTreeSet<Backend>> {
        self.lb.backends().get_backend()
    }

    pub fn select(&self, key: &[u8]) -> Option<Backend> {
        self.lb.select(key, self.config.max_iterations)
    }

    pub async fn update_strategy(&self, new_strategy: Adaptive) -> bool {
        self.lb.update_strategy(new_strategy).await
    }
}
