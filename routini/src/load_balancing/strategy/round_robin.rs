use std::collections::BTreeSet;

use serde::Deserialize;

use crate::load_balancing::{
    Backend,
    strategy::{BackendSelection, Strategy, algorithms, weighted::WeightedSelector},
};

pub type RoundRobinSelector = WeightedSelector<algorithms::RoundRobin>;

#[derive(PartialEq, Default, Deserialize)]
pub struct RoundRobin;

impl Strategy for RoundRobin {
    type BackendSelector = RoundRobinSelector;

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector {
        RoundRobinSelector::build(backends)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::load_balancing::strategy::{BackendIter, BackendSelection};

    use super::*;

    #[test]
    fn test_round_robin() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let mut b2 = Backend::new("1.0.0.1:80").unwrap();
        b2.weight = 8; // 8x than the rest
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        // sorted with: [b2, b3, b1]
        // weighted: [0, 0, 0, 0, 0, 0, 0, 0, 1, 2]
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash = Arc::new(RoundRobin.build_backend_selector(&backends));

        // same hash iter over
        let mut iter = hash.iter(b"test");
        // first, should be weighted
        // weighted: [0, 0, 0, 0, 0, 0, 0, 0, 1, 2]
        //            ^
        assert_eq!(iter.next(), Some(&b2));
        // fallbacks, should be round robin
        assert_eq!(iter.next(), Some(&b3));
        assert_eq!(iter.next(), Some(&b1));
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b3));

        // round robin, ignoring the hash key
        // index advanced 5 steps
        // weighted: [0, 0, 0, 0, 0, 0, 0, 0, 1, 2]
        //                           ^
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b3));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test1");
        // rounded
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b2));
    }
}
