use std::collections::BTreeSet;

use serde::Deserialize;

use crate::load_balancing::{
    Backend,
    strategy::{BackendSelection, Strategy, weighted::WeightedSelector},
};

pub type FNVHashSelector = WeightedSelector<fnv::FnvHasher>;

#[derive(PartialEq, Deserialize)]
pub struct FNVHash;

impl Strategy for FNVHash {
    type BackendSelector = FNVHashSelector;

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector {
        FNVHashSelector::build(backends)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use crate::load_balancing::strategy::{BackendIter, BackendSelection};

    use super::*;

    #[test]
    fn test_fnv() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let mut b2 = Backend::new("1.0.0.1:80").unwrap();
        b2.weight = 10; // 10x than the rest
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash = Arc::new(FNVHash.build_backend_selector(&backends));

        // same hash iter over
        let mut iter = hash.iter(b"test");
        // first, should be weighted
        assert_eq!(iter.next(), Some(&b2));
        // fallbacks, should be uniform, not weighted
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b1));
        assert_eq!(iter.next(), Some(&b3));
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b1));
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b3));
        assert_eq!(iter.next(), Some(&b1));

        // different hashes, the first selection should be weighted
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test2");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test3");
        assert_eq!(iter.next(), Some(&b3));
        let mut iter = hash.iter(b"test4");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test5");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test6");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test7");
        assert_eq!(iter.next(), Some(&b2));
    }
}
