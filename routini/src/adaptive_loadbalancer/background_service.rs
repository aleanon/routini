use std::time::Duration;

use async_trait::async_trait;
use log::error;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use tokio::time::Instant;

use crate::adaptive_loadbalancer::{
    AdaptiveLoadBalancer,
    decision_engine::{AdaptiveDecisionEngine, DecisionEngine},
};

#[async_trait]
impl BackgroundService for AdaptiveLoadBalancer<AdaptiveDecisionEngine> {
    async fn start(&self, shutdown: ShutdownWatch) -> () {
        // 136 years
        const NEVER: Duration = Duration::from_secs(u32::MAX as u64);
        let mut now = Instant::now();
        // run update and health check once
        let mut next_update = now;
        let mut next_health_check = now;
        let mut next_strategy_eval = now;
        let mut selector_rebuild = now + self.lb.rebuild_frequency().await.unwrap_or(NEVER);

        loop {
            if *shutdown.borrow() {
                break;
            }

            if next_update <= now {
                if let Err(err) = self.lb.update().await {
                    error!("Failed to update load balancer: {}", err);
                }
                next_update = now + self.lb.update_frequency.unwrap_or(NEVER);
            }

            if next_health_check <= now {
                self.lb
                    .backends()
                    .run_health_check(self.lb.parallel_health_check)
                    .await;
                next_health_check = now + self.lb.health_check_frequency.unwrap_or(NEVER);
            }

            if next_strategy_eval <= now {
                let current_strategy = self.lb.current_strategy().await;
                let backends = self.lb.backends().get_backend();
                let strategy = self
                    .decision_engine
                    .evaluate_strategy(&current_strategy, &backends);

                let was_updated = self.lb.update_strategy(strategy).await;
                next_strategy_eval = now + self.decision_engine.evaluate_strategy_frequency;
                if was_updated {
                    selector_rebuild = now + self.lb.rebuild_frequency().await.unwrap_or(NEVER);
                }
            }

            if selector_rebuild <= now {
                self.lb.rebuild_selector().await;
                selector_rebuild = now + self.lb.rebuild_frequency().await.unwrap_or(NEVER);
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

            tokio::time::sleep_until(to_wake.into()).await;
            now = Instant::now();
        }
    }
}
