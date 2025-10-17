use std::{collections::BTreeSet, sync::Arc};

use crate::load_balancing::{
    Backend,
    selection::{
        BackendIter, BackendSelection, ConsistentSelector, FNVHashSelector, RandomSelector,
        RoundRobin, RoundRobinSelector, SelectorBuilder,
        least_connections::LeastConnectionsSelector,
    },
};

#[derive(Default, PartialEq)]
pub enum Adaptive {
    #[default]
    RoundRobin,
    Random,
    FNVHash,
    Consistent,
    LeastConnections,
}

impl SelectorBuilder for Adaptive {
    type Selector = AdaptiveSelector;

    fn build_selector(&self, backends: &BTreeSet<Backend>) -> Self::Selector {
        match self {
            Adaptive::RoundRobin => AdaptiveSelector::RoundRobin(Arc::new(
                RoundRobin::default().build_selector(backends),
            )),
            Adaptive::Random => AdaptiveSelector::Random(Arc::new(RandomSelector::build(backends))),
            Adaptive::FNVHash => {
                AdaptiveSelector::FNVHash(Arc::new(FNVHashSelector::build(backends)))
            }
            Adaptive::Consistent => {
                AdaptiveSelector::Consistent(Arc::new(ConsistentSelector::build(backends)))
            }
            Adaptive::LeastConnections => AdaptiveSelector::LeastConnections(Arc::new(
                LeastConnectionsSelector::build(backends),
            )),
        }
    }
}

pub enum AdaptiveSelector {
    RoundRobin(Arc<RoundRobinSelector>),
    Random(Arc<RandomSelector>),
    FNVHash(Arc<FNVHashSelector>),
    Consistent(Arc<ConsistentSelector>),
    LeastConnections(Arc<LeastConnectionsSelector>),
}

impl BackendSelection for AdaptiveSelector {
    type Iter = AdaptiveIter;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        Self::RoundRobin(Arc::new(RoundRobinSelector::build(backends)))
    }

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
    LeastConnections(<LeastConnectionsSelector as BackendSelection>::Iter),
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
