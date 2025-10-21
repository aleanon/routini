use std::collections::BTreeSet;

use serde::Deserialize;

use crate::load_balancing::{
    Backend,
    strategy::{BackendSelection, Strategy, algorithms, weighted::WeightedSelector},
};

pub type RandomSelector = WeightedSelector<algorithms::Random>;

#[derive(PartialEq, Default, Deserialize)]
pub struct Random;

impl Strategy for Random {
    type BackendSelector = RandomSelector;

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector {
        RandomSelector::build(backends)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use crate::load_balancing::strategy::{BackendIter, BackendSelection};

    use super::*;

    #[test]
    fn test_random() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let mut b2 = Backend::new("1.0.0.1:80").unwrap();
        b2.weight = 8; // 8x than the rest
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash = Arc::new(Random.build_backend_selector(&backends));

        let mut count = HashMap::new();
        count.insert(b1.clone(), 0);
        count.insert(b2.clone(), 0);
        count.insert(b3.clone(), 0);

        for _ in 0..10000 {
            let mut iter = hash.iter(b"test");
            *count.get_mut(iter.next().unwrap()).unwrap() += 1;
        }
        let b2_count = *count.get(&b2).unwrap();
        assert!((7000..=9000).contains(&b2_count));
    }
}
