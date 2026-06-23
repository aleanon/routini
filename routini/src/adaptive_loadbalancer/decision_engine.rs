use std::{collections::BTreeSet, sync::Arc, time::Duration};

use crate::{
    adaptive_loadbalancer::{AdaptiveBackend, options::AdaptiveLbOpt},
    load_balancing::{
        Metrics,
        strategy::{Adaptive, Strategy},
    },
};

pub trait DecisionEngine {
    type Strategy: Strategy;

    fn evaluate_strategy(
        &self,
        current_strategy: &Self::Strategy,
        backends: &Arc<BTreeSet<AdaptiveBackend>>,
    ) -> Self::Strategy;
}

pub struct AdaptiveDecisionEngine {
    pub evaluate_strategy_frequency: Duration,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    /// The minimum number of connections needed on the backend with most connections to make use
    /// of the fewest connections strategy.
    pub min_nr_of_connections: usize,
    /// Fraction of the enter thresholds used as exit thresholds. Provides hysteresis so the
    /// engine does not oscillate between strategies on every evaluation cycle.
    pub hysteresis_exit_factor: f32,
}

impl AdaptiveDecisionEngine {
    pub fn new(opt: &AdaptiveLbOpt) -> Self {
        Self {
            evaluate_strategy_frequency: opt.evaluate_strategy_frequency,
            connections_divergence_ratio: opt.connections_divergence_ratio,
            latency_divergence_ratio: opt.latency_divergence_ratio,
            min_nr_of_connections: opt.min_nr_of_connections,
            hysteresis_exit_factor: opt.hysteresis_exit_factor,
        }
    }

    /// Returns the spread between the slowest and fastest backend latencies (`max / min`),
    /// considering only backends that have actually recorded a latency sample (> 0).
    ///
    /// Backends with no measurement yet are ignored rather than counted as `0.0`, which would
    /// otherwise mask real divergence (a single unmeasured backend would force the ratio to 1.0).
    /// Returns `None` when fewer than two backends have usable measurements.
    fn latency_divergence(&self, backends: &Arc<BTreeSet<AdaptiveBackend>>) -> Option<f32> {
        let mut min = f32::MAX;
        let mut max = f32::MIN;
        let mut measured = 0usize;

        for backend in backends.iter() {
            let latency = backend.metrics.average_latency().unwrap_or(0.0);
            if latency > 0.0 {
                measured += 1;
                min = min.min(latency);
                max = max.max(latency);
            }
        }

        if measured < 2 || min <= 0.0 {
            return None;
        }

        Some(max / min)
    }

    /// Returns `(ratio, max_connections)` describing the imbalance in active connections across
    /// backends. `ratio` is `max / min`; when the least-loaded backend has zero connections it is
    /// treated as the maximum spread (`max as f32`) to avoid a division by zero that would
    /// otherwise report infinite divergence.
    ///
    /// Returns `None` when fewer than two backends report connection counts (i.e. the active
    /// strategy is not tracking connections).
    fn connection_divergence(&self, backends: &Arc<BTreeSet<AdaptiveBackend>>) -> Option<(f32, usize)> {
        let mut min = usize::MAX;
        let mut max = 0usize;
        let mut tracked = 0usize;

        for backend in backends.iter() {
            if let Some(connections) = backend.metrics.active_connections() {
                tracked += 1;
                min = min.min(connections);
                max = max.max(connections);
            }
        }

        if tracked < 2 {
            return None;
        }

        let ratio = if min == 0 {
            max as f32
        } else {
            max as f32 / min as f32
        };

        Some((ratio, max))
    }
}

impl DecisionEngine for AdaptiveDecisionEngine {
    type Strategy = Adaptive;

    /// Chooses the strategy to run based on observed backend metrics.
    ///
    /// Priority order:
    /// 1. **Connection imbalance** (overload protection): if the busiest backend has crossed
    ///    `min_nr_of_connections` and connections are skewed, switch to `FewestConnections`.
    /// 2. **Latency divergence**: if some backends are markedly slower, switch to `FastestServer`.
    /// 3. Otherwise fall back to `RoundRobin`.
    ///
    /// Each signal uses a higher *enter* threshold than *exit* threshold (hysteresis): once a
    /// strategy is active it stays active until the metric improves past the lower exit threshold,
    /// preventing rapid flapping (and the selector rebuilds that come with it).
    fn evaluate_strategy(
        &self,
        current_strategy: &Self::Strategy,
        backends: &Arc<BTreeSet<AdaptiveBackend>>,
    ) -> Self::Strategy {
        if backends.len() < 2 {
            return Adaptive::RoundRobin;
        }

        let exit = self.hysteresis_exit_factor;

        // 1. Connection imbalance takes priority as a guard against overloading a single backend.
        if let Some((ratio, max)) = self.connection_divergence(backends) {
            if max >= self.min_nr_of_connections {
                let threshold = if *current_strategy == Adaptive::FewestConnections {
                    self.connections_divergence_ratio * exit
                } else {
                    self.connections_divergence_ratio
                };
                if ratio > threshold {
                    return Adaptive::FewestConnections;
                }
            }
        }

        // 2. Latency divergence: route more traffic to the faster backends.
        if let Some(ratio) = self.latency_divergence(backends) {
            let threshold = if *current_strategy == Adaptive::FastestServer {
                self.latency_divergence_ratio * exit
            } else {
                self.latency_divergence_ratio
            };
            if ratio > threshold {
                return Adaptive::FastestServer;
            }
        }

        // 3. Balanced enough: even distribution.
        Adaptive::RoundRobin
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use std::time::Duration;

    use crate::adaptive_loadbalancer::options::AdaptiveLbOpt;

    use super::*;

    fn engine() -> AdaptiveDecisionEngine {
        AdaptiveDecisionEngine::new(&AdaptiveLbOpt {
            // small connection floor so tests don't need thousands of connections
            min_nr_of_connections: 1,
            ..Default::default()
        })
    }

    fn backend_with_latency(addr: &str, latency_ms: f32) -> AdaptiveBackend {
        let backend = AdaptiveBackend::build(addr, 1).unwrap();
        if latency_ms > 0.0 {
            // alpha 1.0 with a zero starting average sets the EWMA directly to latency_ms.
            backend
                .metrics
                .record_latency(Duration::from_secs_f32(latency_ms / 1000.0), 1.0);
        }
        backend
    }

    fn backend_with_connections(addr: &str, connections: usize) -> AdaptiveBackend {
        let backend = AdaptiveBackend::build(addr, 1).unwrap();
        for _ in 0..connections {
            backend.metrics.increment_active_connections();
        }
        backend
    }

    fn set(backends: impl IntoIterator<Item = AdaptiveBackend>) -> Arc<BTreeSet<AdaptiveBackend>> {
        Arc::new(backends.into_iter().collect())
    }

    #[test]
    fn single_backend_stays_round_robin() {
        let engine = engine();
        let backends = set([backend_with_latency("127.0.0.1:8080", 500.0)]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::RoundRobin
        );
    }

    #[test]
    fn unmeasured_backend_does_not_mask_latency_divergence() {
        let engine = engine();
        // One slow, one fast, one not yet measured (0.0). The unmeasured one must be ignored
        // so the 10x spread between the measured pair is still detected.
        let backends = set([
            backend_with_latency("127.0.0.1:8080", 100.0),
            backend_with_latency("127.0.0.1:8081", 10.0),
            backend_with_latency("127.0.0.1:8082", 0.0),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::FastestServer
        );
    }

    #[test]
    fn zero_connection_backend_does_not_force_infinite_ratio_panic() {
        let engine = engine();
        // min == 0 must not divide-by-zero; a large spread should select FewestConnections.
        let backends = set([
            backend_with_connections("127.0.0.1:8080", 50),
            backend_with_connections("127.0.0.1:8081", 0),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::FewestConnections
        );
    }

    #[test]
    fn balanced_backends_stay_round_robin() {
        let engine = engine();
        let backends = set([
            backend_with_connections("127.0.0.1:8080", 10),
            backend_with_connections("127.0.0.1:8081", 10),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::RoundRobin
        );
    }

    #[test]
    fn hysteresis_keeps_fastest_server_until_below_exit_threshold() {
        let engine = engine();
        // latency_divergence_ratio default 2.0, exit factor 0.75 -> exit threshold 1.5.
        // Ratio of 1.6 is below the enter threshold (2.0) but above the exit threshold (1.5),
        // so an engine already in FastestServer must stay there, while one in RoundRobin must
        // not switch in.
        let backends = set([
            backend_with_latency("127.0.0.1:8080", 16.0),
            backend_with_latency("127.0.0.1:8081", 10.0),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::FastestServer, &backends),
            Adaptive::FastestServer
        );
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::RoundRobin
        );
    }
}
