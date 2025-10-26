use std::{collections::BTreeSet, sync::Arc};

use serde::Deserialize;

use crate::load_balancing::{
    Backend,
    strategy::{
        BackendIter, BackendSelection, Consistent, FNVHash, FewestConnections, Random, RoundRobin,
        Strategy, consistent::ConsistentSelector, fewest_connections::FewestConnectionsSelector,
        fnv_hash::FNVHashSelector, random::RandomSelector, round_robin::RoundRobinSelector,
    },
};

#[derive(Default, PartialEq, Deserialize, Clone)]
pub enum Adaptive {
    RoundRobin,
    Random,
    #[default]
    FNVHash,
    Consistent,
    FewestConnections,
}

impl Strategy for Adaptive {
    type BackendSelector = AdaptiveSelector;

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
            Adaptive::FewestConnections => AdaptiveSelector::LeastConnections(Arc::new(
                FewestConnections.build_backend_selector(backends),
            )),
        }
    }
}

pub enum AdaptiveSelector {
    RoundRobin(Arc<RoundRobinSelector>),
    Random(Arc<RandomSelector>),
    FNVHash(Arc<FNVHashSelector>),
    Consistent(Arc<ConsistentSelector>),
    LeastConnections(Arc<FewestConnectionsSelector>),
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
            AdaptiveSelector::LeastConnections(selector) => {
                AdaptiveIter::LeastConnections(selector.iter(key))
            }
        }
    }
}

pub enum AdaptiveIter {
    RoundRobin(<RoundRobinSelector as BackendSelection>::Iter),
    Random(<RandomSelector as BackendSelection>::Iter),
    FNVHash(<FNVHashSelector as BackendSelection>::Iter),
    Consistent(<ConsistentSelector as BackendSelection>::Iter),
    LeastConnections(<FewestConnectionsSelector as BackendSelection>::Iter),
}

impl BackendIter for AdaptiveIter {
    fn next(&mut self) -> Option<&Backend> {
        match self {
            AdaptiveIter::RoundRobin(iter) => iter.next(),
            AdaptiveIter::Random(iter) => iter.next(),
            AdaptiveIter::FNVHash(iter) => iter.next(),
            AdaptiveIter::Consistent(iter) => iter.next(),
            AdaptiveIter::LeastConnections(iter) => iter.next(),
        }
    }
}
