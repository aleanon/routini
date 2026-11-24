use std::time::Duration;

use async_trait::async_trait;
use log::{error, info};
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use tokio::time::Instant;

use crate::adaptive_loadbalancer::{
    AdaptiveLoadBalancer,
    decision_engine::{AdaptiveDecisionEngine, DecisionEngine},
};

#[async_trait]
impl BackgroundService for AdaptiveLoadBalancer<AdaptiveDecisionEngine> {
    async fn start(&self, mut shutdown: ShutdownWatch) -> () {
        // 136 years
        const NEVER: Duration = Duration::from_secs(u32::MAX as u64);
        let mut now = Instant::now();
        // run update and health check once
        let mut next_update = now;
        let mut next_health_check = now;
        let mut next_strategy_eval = now;
        let mut selector_rebuild = now + self.rebuild_interval().await.unwrap_or(NEVER);

        loop {
            if *shutdown.borrow() {
                info!("Shutdown signal received, stopping adaptive load balancer background tasks");
                break;
            }

            if next_update <= now {
                if let Err(err) = self.lb.update().await {
                    error!("Failed to update load balancer: {}", err);
                }
                next_update = now + self.config().discovery_interval.unwrap_or(NEVER);
            }

            if next_health_check <= now {
                self.lb
                    .backends()
                    .run_health_check(self.lb.parallel_health_check)
                    .await;
                next_health_check = now + self.config().health_check_interval.unwrap_or(NEVER);
            }

            if next_strategy_eval <= now {
                let current_strategy = self.lb.current_strategy().await;
                let backends = self.lb.backends().get_backend();
                let strategy = self
                    .decision_engine
                    .evaluate_strategy(&current_strategy, &backends);

                let was_updated = self.lb.update_strategy(strategy).await;
                next_strategy_eval = now + self.config().evaluate_strategy_frequency;
                if was_updated {
                    selector_rebuild = now + self.rebuild_interval().await.unwrap_or(NEVER);
                }
            }

            if selector_rebuild <= now {
                self.lb.rebuild_selector().await;
                selector_rebuild = now + self.rebuild_interval().await.unwrap_or(NEVER);
            }

            let Some(to_wake) = [
                next_update,
                next_health_check,
                next_strategy_eval,
                selector_rebuild,
            ]
            .iter()
            .min()
            .cloned() else {
                unreachable!("Array contained no value")
            };

            // Wake on whichever comes first, so a shutdown is not delayed by a long sleep
            tokio::select! {
                _ = tokio::time::sleep_until(to_wake) => {}
                // Config reloaded: run discovery, health check and evaluation now, rescheduling
                // them with the new intervals
                _ = self.wait_reconfigured() => {
                    next_update = Instant::now();
                    next_health_check = Instant::now();
                    next_strategy_eval = Instant::now();
                    selector_rebuild = Instant::now();
                }
                changed = shutdown.changed() => {
                    // Err means the sender is gone, which only happens when the server is
                    // being torn down, treat it as a shutdown as well.
                    if changed.is_err() {
                        break;
                    }
                }
            }
            now = Instant::now();
        }
    }
}

impl AdaptiveLoadBalancer<AdaptiveDecisionEngine> {
    /// How long until the selector is rebuilt again: the route's interval for a strategy that
    /// re-sorts by live metrics, `None` for one that doesn't.
    async fn rebuild_interval(&self) -> Option<Duration> {
        self.lb
            .rebuild_frequency()
            .await
            .map(|_| self.config().selector_rebuild_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adaptive_loadbalancer::options::AdaptiveLbOpt,
        load_balancing::{Backends, discovery::Static, strategy::Adaptive},
    };

    fn lb(strategy: Adaptive) -> AdaptiveLoadBalancer<AdaptiveDecisionEngine> {
        let opt = AdaptiveLbOpt {
            starting_strategy: strategy,
            selector_rebuild_interval: Duration::from_millis(50),
            health_check_interval: None,
            ..Default::default()
        };
        let engine = AdaptiveDecisionEngine::new(&opt);
        let discovery = Static::try_from_iter(["127.0.0.1:7001"]).unwrap();
        AdaptiveLoadBalancer::from_backends(Backends::new(discovery), Some(opt), engine)
    }

    #[tokio::test]
    async fn selectors_are_rebuilt_at_the_routes_interval_when_metrics_drive_them() {
        for strategy in [Adaptive::FewestConnections, Adaptive::FastestServer] {
            assert_eq!(
                lb(strategy).rebuild_interval().await,
                Some(Duration::from_millis(50))
            );
        }
        assert_eq!(lb(Adaptive::RoundRobin).rebuild_interval().await, None);
    }
}
