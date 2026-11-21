use std::{collections::BTreeSet, sync::Arc};

use arc_swap::ArcSwap;

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

    /// Change the strategy the engine falls back to when no adaptive condition holds.
    /// Called when the strategy is overridden manually. No-op by default.
    fn set_default_strategy(&self, _strategy: Self::Strategy) {}

    /// Apply new tuning options, called on config reload. No-op by default.
    fn reconfigure(&self, _options: &AdaptiveLbOpt) {}
}

/// Thresholds of [AdaptiveDecisionEngine], swapped as a whole on reconfigure.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineThresholds {
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    /// The minimum number of connections needed on the backend with most connections to make use
    /// of the fewest connections strategy.
    pub min_nr_of_connections: usize,
    /// Fraction of the enter thresholds used as exit thresholds. Provides hysteresis so the
    /// engine does not oscillate between strategies on every evaluation cycle.
    pub hysteresis_exit_factor: f32,
}

impl From<&AdaptiveLbOpt> for EngineThresholds {
    fn from(opt: &AdaptiveLbOpt) -> Self {
        Self {
            connections_divergence_ratio: opt.connections_divergence_ratio,
            latency_divergence_ratio: opt.latency_divergence_ratio,
            min_nr_of_connections: opt.min_nr_of_connections,
            hysteresis_exit_factor: opt.hysteresis_exit_factor,
        }
    }
}

pub struct AdaptiveDecisionEngine {
    thresholds: ArcSwap<EngineThresholds>,
    /// Strategy to return to when backends are balanced. Starts as the configured starting
    /// strategy, replaced by manual overrides.
    default_strategy: ArcSwap<Adaptive>,
}

impl AdaptiveDecisionEngine {
    pub fn new(opt: &AdaptiveLbOpt) -> Self {
        Self {
            thresholds: ArcSwap::from_pointee(EngineThresholds::from(opt)),
            default_strategy: ArcSwap::from_pointee(opt.starting_strategy.clone()),
        }
    }

    pub fn thresholds(&self) -> Arc<EngineThresholds> {
        self.thresholds.load_full()
    }

    /// The strategy used when no adaptive condition holds.
    pub fn default_strategy(&self) -> Adaptive {
        (**self.default_strategy.load()).clone()
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
    fn connection_divergence(
        &self,
        backends: &Arc<BTreeSet<AdaptiveBackend>>,
    ) -> Option<(f32, usize)> {
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

/// The threshold a ratio must fall to before an active strategy is left again.
///
/// Divergence ratios are `max / min`, so they are never below 1.0. The hysteresis factor is
/// therefore applied to the part of the enter threshold above 1.0: with an enter threshold of 1.2
/// and a factor of 0.75 the exit threshold is 1.15. Scaling the whole threshold (1.2 * 0.75 = 0.9)
/// would put it below any reachable ratio and make the strategy impossible to leave.
fn exit_threshold(enter: f32, factor: f32) -> f32 {
    1.0 + (enter - 1.0) * factor
}

impl DecisionEngine for AdaptiveDecisionEngine {
    type Strategy = Adaptive;

    /// Chooses the strategy to run based on observed backend metrics.
    ///
    /// Priority order:
    /// 1. **Connection imbalance** (overload protection): if the busiest backend has crossed
    ///    `min_nr_of_connections` and connections are skewed, switch to `FewestConnections`.
    /// 2. **Latency divergence**: if some backends are markedly slower, switch to `FastestServer`.
    /// 3. Otherwise fall back to the default strategy: the route's configured starting strategy,
    ///    or the last manual override.
    ///
    /// A hash strategy as the default is kept regardless of metrics: clients rely on reaching
    /// the same backend (sessions, caches), and the load-based strategies ignore the key.
    ///
    /// Each signal uses a higher *enter* threshold than *exit* threshold (hysteresis): once a
    /// strategy is active it stays active until the metric improves past the lower exit threshold,
    /// preventing rapid flapping (and the selector rebuilds that come with it).
    fn evaluate_strategy(
        &self,
        current_strategy: &Self::Strategy,
        backends: &Arc<BTreeSet<AdaptiveBackend>>,
    ) -> Self::Strategy {
        let default = self.default_strategy();
        if backends.len() < 2 || default.is_key_affine() {
            return default;
        }

        let t = self.thresholds.load();
        let exit = t.hysteresis_exit_factor;

        // 1. Connection imbalance takes priority as a guard against overloading a single backend.
        if let Some((ratio, max)) = self.connection_divergence(backends) {
            if max >= t.min_nr_of_connections {
                let threshold = if *current_strategy == Adaptive::FewestConnections {
                    exit_threshold(t.connections_divergence_ratio, exit)
                } else {
                    t.connections_divergence_ratio
                };
                if ratio > threshold {
                    return Adaptive::FewestConnections;
                }
            }
        }

        // 2. Latency divergence: route more traffic to the faster backends.
        if let Some(ratio) = self.latency_divergence(backends) {
            let threshold = if *current_strategy == Adaptive::FastestServer {
                exit_threshold(t.latency_divergence_ratio, exit)
            } else {
                t.latency_divergence_ratio
            };
            if ratio > threshold {
                return Adaptive::FastestServer;
            }
        }

        // 3. Balanced enough: the default strategy.
        default
    }

    fn set_default_strategy(&self, strategy: Self::Strategy) {
        self.default_strategy.store(Arc::new(strategy));
    }

    fn reconfigure(&self, options: &AdaptiveLbOpt) {
        self.thresholds
            .store(Arc::new(EngineThresholds::from(options)));
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

    fn engine_with_default(default: Adaptive) -> AdaptiveDecisionEngine {
        AdaptiveDecisionEngine::new(&AdaptiveLbOpt {
            min_nr_of_connections: 1,
            starting_strategy: default,
            ..Default::default()
        })
    }

    #[test]
    fn balanced_backends_fall_back_to_the_starting_strategy() {
        let backends = set([
            backend_with_connections("127.0.0.1:8080", 10),
            backend_with_connections("127.0.0.1:8081", 10),
        ]);
        for default in [
            Adaptive::RoundRobin,
            Adaptive::Consistent,
            Adaptive::FNVHash,
        ] {
            let engine = engine_with_default(default.clone());
            for current in [
                Adaptive::FastestServer,
                Adaptive::FewestConnections,
                Adaptive::Random,
                default.clone(),
            ] {
                assert_eq!(
                    engine.evaluate_strategy(&current, &backends),
                    default,
                    "from {current}"
                );
            }
        }
    }

    #[test]
    fn hash_strategies_are_never_switched_away_from() {
        let engine = engine();
        let skewed = set([
            backend_with_latency("127.0.0.1:8080", 1.0),
            backend_with_latency("127.0.0.1:8081", 500.0),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &skewed),
            Adaptive::FastestServer
        );
        for hash in [Adaptive::FNVHash, Adaptive::Consistent] {
            let engine = engine_with_default(hash.clone());
            assert_eq!(engine.evaluate_strategy(&hash, &skewed), hash);
            // Also leaves a load-based strategy a manual override had replaced it with
            assert_eq!(
                engine.evaluate_strategy(&Adaptive::FastestServer, &skewed),
                hash
            );
        }
    }

    #[test]
    fn single_backend_uses_the_starting_strategy() {
        let engine = engine_with_default(Adaptive::Consistent);
        let backends = set([backend_with_latency("127.0.0.1:8080", 500.0)]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::FastestServer, &backends),
            Adaptive::Consistent
        );
    }

    #[test]
    fn set_default_strategy_changes_the_fallback() {
        let engine = engine_with_default(Adaptive::Consistent);
        engine.set_default_strategy(Adaptive::Random);
        assert_eq!(engine.default_strategy(), Adaptive::Random);
        let backends = set([
            backend_with_connections("127.0.0.1:8080", 10),
            backend_with_connections("127.0.0.1:8081", 10),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::FastestServer, &backends),
            Adaptive::Random
        );
    }

    #[test]
    fn fewest_connections_is_left_once_connections_even_out() {
        let engine = engine();
        // Default enter 1.2, exit 1.15. 24/20 = 1.2 is above the exit threshold: stays when
        // active, while not entering from RoundRobin (not above the enter threshold).
        let inside_band = set([
            backend_with_connections("127.0.0.1:8080", 24),
            backend_with_connections("127.0.0.1:8081", 20),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::FewestConnections, &inside_band),
            Adaptive::FewestConnections
        );
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &inside_band),
            Adaptive::RoundRobin
        );
        // Balanced: must fall back. With exit = 1.2 * 0.75 = 0.9 this stayed forever.
        let balanced = set([
            backend_with_connections("127.0.0.1:8080", 20),
            backend_with_connections("127.0.0.1:8081", 20),
        ]);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::FewestConnections, &balanced),
            Adaptive::RoundRobin
        );
    }

    #[test]
    fn exit_threshold_stays_above_one() {
        for (enter, factor, expected) in [
            (1.2, 0.75, 1.15),
            (2.0, 0.75, 1.75),
            (2.0, 1.0, 2.0),
            (2.0, 0.0, 1.0),
        ] {
            let got = exit_threshold(enter, factor);
            assert!((got - expected).abs() < 1e-6, "{enter} * {factor}: {got}");
        }
    }

    #[test]
    fn reconfigure_changes_thresholds() {
        let engine = AdaptiveDecisionEngine::new(&AdaptiveLbOpt::default());
        let backends = set([
            backend_with_connections("127.0.0.1:8080", 50),
            backend_with_connections("127.0.0.1:8081", 10),
        ]);
        // Default min_nr_of_connections (1000) is not reached
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::RoundRobin
        );
        engine.reconfigure(&AdaptiveLbOpt {
            min_nr_of_connections: 10,
            ..Default::default()
        });
        assert_eq!(engine.thresholds().min_nr_of_connections, 10);
        assert_eq!(
            engine.evaluate_strategy(&Adaptive::RoundRobin, &backends),
            Adaptive::FewestConnections
        );
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
        // latency_divergence_ratio default 2.0, exit factor 0.75 -> exit threshold 1.75.
        // Ratio of 1.8 is below the enter threshold (2.0) but above the exit threshold (1.75),
        // so an engine already in FastestServer must stay there, while one in RoundRobin must
        // not switch in.
        let backends = set([
            backend_with_latency("127.0.0.1:8080", 18.0),
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
