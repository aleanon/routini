use std::{collections::BTreeSet, fmt::Display, sync::Arc, time::Duration};

use serde::Deserialize;

use crate::load_balancing::{
    Backend, Metrics, NoMetric,
    strategy::{
        BackendIter, BackendSelection, Consistent, FNVHash, FewestConnections, Random, RoundRobin,
        Strategy,
        consistent::ConsistentSelector,
        fastest_server::{FastestServer, FastestServerSelector, LatencyEWMA},
        fewest_connections::{ActiveConnections, FewestConnectionsSelector},
        fnv_hash::FNVHashSelector,
        random::RandomSelector,
        round_robin::RoundRobinSelector,
    },
};

#[derive(Debug, Default, PartialEq, Clone, Deserialize)]
pub enum Adaptive {
    #[default]
    RoundRobin,
    Random,
    FNVHash,
    Consistent,
    FewestConnections,
    FastestServer,
}

impl Display for Adaptive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Adaptive::RoundRobin => write!(f, "RoundRobin"),
            Adaptive::Random => write!(f, "Random"),
            Adaptive::FNVHash => write!(f, "FNVHash"),
            Adaptive::FewestConnections => write!(f, "FewestConnection"),
            Adaptive::Consistent => write!(f, "Consistent"),
            Adaptive::FastestServer => write!(f, "FastestServer"),
        }
    }
}

impl<M: Metrics> Strategy<M> for Adaptive {
    type BackendSelector = AdaptiveSelector<M>;

    fn rebuild_frequency(&self) -> Option<Duration> {
        match self {
            Adaptive::FewestConnections | Adaptive::FastestServer => {
                Some(Duration::from_millis(200))
            }
            _ => None,
        }
    }

    fn build_backend_selector(&self, backends: &BTreeSet<Backend<M>>) -> Self::BackendSelector {
        match self {
            Adaptive::RoundRobin => {
                AdaptiveSelector::RoundRobin(Arc::new(RoundRobin.build_backend_selector(backends)))
            }
            Adaptive::Random => {
                AdaptiveSelector::Random(Arc::new(Random.build_backend_selector(backends)))
            }
            Adaptive::FNVHash => {
                AdaptiveSelector::FNVHash(Arc::new(FNVHash.build_backend_selector(backends)))
            }
            Adaptive::Consistent => {
                AdaptiveSelector::Consistent(Arc::new(Consistent.build_backend_selector(backends)))
            }
            Adaptive::FewestConnections => AdaptiveSelector::FewestConnections(Arc::new(
                FewestConnections.build_backend_selector(backends),
            )),
            Adaptive::FastestServer => AdaptiveSelector::FastestServer(Arc::new(
                FastestServer.build_backend_selector(backends),
            )),
        }
    }
}

pub enum AdaptiveSelector<M: Metrics = NoMetric> {
    RoundRobin(Arc<RoundRobinSelector<M>>),
    Random(Arc<RandomSelector<M>>),
    FNVHash(Arc<FNVHashSelector<M>>),
    Consistent(Arc<ConsistentSelector<M>>),
    FewestConnections(Arc<FewestConnectionsSelector<M>>),
    FastestServer(Arc<FastestServerSelector<M>>),
}

impl<M: Metrics> BackendSelection<M> for AdaptiveSelector<M> {
    type Iter = AdaptiveIter<M>;

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        match &**self {
            AdaptiveSelector::RoundRobin(selector) => AdaptiveIter::RoundRobin(selector.iter(key)),
            AdaptiveSelector::Random(selector) => AdaptiveIter::Random(selector.iter(key)),
            AdaptiveSelector::FNVHash(selector) => AdaptiveIter::FNVHash(selector.iter(key)),
            AdaptiveSelector::Consistent(selector) => AdaptiveIter::Consistent(selector.iter(key)),
            AdaptiveSelector::FewestConnections(selector) => {
                AdaptiveIter::FewestConnections(selector.iter(key))
            }
            AdaptiveSelector::FastestServer(selector) => {
                AdaptiveIter::FastestServer(selector.iter(key))
            }
        }
    }
}

pub enum AdaptiveIter<M: Metrics = NoMetric> {
    RoundRobin(<RoundRobinSelector<M> as BackendSelection<M>>::Iter),
    Random(<RandomSelector<M> as BackendSelection<M>>::Iter),
    FNVHash(<FNVHashSelector<M> as BackendSelection<M>>::Iter),
    Consistent(<ConsistentSelector<M> as BackendSelection<M>>::Iter),
    FewestConnections(<FewestConnectionsSelector<M> as BackendSelection<M>>::Iter),
    FastestServer(<FastestServerSelector<M> as BackendSelection<M>>::Iter),
}

impl<M: Metrics> BackendIter<M> for AdaptiveIter<M> {
    fn next(&mut self) -> Option<&Backend<M>> {
        match self {
            AdaptiveIter::RoundRobin(iter) => iter.next(),
            AdaptiveIter::Random(iter) => iter.next(),
            AdaptiveIter::FNVHash(iter) => iter.next(),
            AdaptiveIter::Consistent(iter) => iter.next(),
            AdaptiveIter::FewestConnections(iter) => iter.next(),
            AdaptiveIter::FastestServer(iter) => iter.next(),
        }
    }
}

/// Combined metrics (active connections + latency EWMA) used by the adaptive load balancer.
/// `Clone`/`Default` derive through the `Arc`-shared inner metric types, so cloning a backend
/// keeps all clones pointing at the same counters.
#[derive(Debug, Clone, Default)]
pub struct AdaptiveStrategyMetrics {
    active_connections: ActiveConnections,
    latency_ewma: LatencyEWMA,
}

impl AdaptiveStrategyMetrics {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Metrics for AdaptiveStrategyMetrics {
    fn increment_active_connections(&self) {
        self.active_connections.increment_active_connections();
    }

    fn decrement_active_connections(&self) {
        self.active_connections.decrement_active_connections();
    }

    fn record_latency(&self, latency: Duration, alpha: f32) {
        self.latency_ewma.record_latency(latency, alpha);
    }

    fn active_connections(&self) -> Option<usize> {
        self.active_connections.active_connections()
    }

    fn average_latency(&self) -> Option<f32> {
        self.latency_ewma.average_latency()
    }
}
