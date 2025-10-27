use std::{collections::BTreeSet, fmt::Display, sync::Arc, time::Duration};

use serde::Deserialize;

use crate::load_balancing::{
    Backend, Metrics,
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

impl Strategy for Adaptive {
    type BackendSelector = AdaptiveSelector;

    fn metrics(&self) -> Option<Arc<dyn Metrics>> {
        Some(Arc::new(AdaptiveStrategyMetrics::new()) as Arc<dyn Metrics>)
    }

    fn rebuild_frequency(&self) -> Option<Duration> {
        match self {
            Adaptive::FewestConnections => FewestConnections.rebuild_frequency(),
            Adaptive::FastestServer => FastestServer.rebuild_frequency(),
            _ => None,
        }
    }

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector {
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

pub enum AdaptiveSelector {
    RoundRobin(Arc<RoundRobinSelector>),
    Random(Arc<RandomSelector>),
    FNVHash(Arc<FNVHashSelector>),
    Consistent(Arc<ConsistentSelector>),
    FewestConnections(Arc<FewestConnectionsSelector>),
    FastestServer(Arc<<FastestServer as Strategy>::BackendSelector>),
}

impl BackendSelection for AdaptiveSelector {
    type Iter = AdaptiveIter;

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter
    where
        Self::Iter: BackendIter,
    {
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

pub enum AdaptiveIter {
    RoundRobin(<RoundRobinSelector as BackendSelection>::Iter),
    Random(<RandomSelector as BackendSelection>::Iter),
    FNVHash(<FNVHashSelector as BackendSelection>::Iter),
    Consistent(<ConsistentSelector as BackendSelection>::Iter),
    FewestConnections(<FewestConnectionsSelector as BackendSelection>::Iter),
    FastestServer(<FastestServerSelector as BackendSelection>::Iter),
}

impl BackendIter for AdaptiveIter {
    fn next(&mut self) -> Option<&Backend> {
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

#[derive(Debug)]
pub struct AdaptiveStrategyMetrics {
    active_connections: ActiveConnections,
    latency_ewma: LatencyEWMA,
}

impl AdaptiveStrategyMetrics {
    pub fn new() -> Self {
        Self {
            active_connections: ActiveConnections::new(0),
            latency_ewma: LatencyEWMA::new(0.0),
        }
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
