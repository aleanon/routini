// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use fnv::FnvHasher;

use crate::load_balancing::{
    Backend,
    strategy::{BackendIter, BackendSelection, SelectionAlgorithm},
};

/// Weighted selection with a given selection algorithm
///
/// The default algorithm is [FnvHasher]. See [super::algorithms] for more choices.
pub struct WeightedSelector<H = FnvHasher> {
    backends: Box<[Backend]>,
    // each item is an index to the `backends`, use u16 to save memory, support up to 2^16 backends
    weighted: Box<[u16]>,
    algorithm: H,
}

impl<H: SelectionAlgorithm + Send + Sync> WeightedSelector<H> {
    pub fn new(backends: &BTreeSet<Backend>) -> Self {
        assert!(
            backends.len() <= u16::MAX as usize,
            "support up to 2^16 backends"
        );
        let backends = Vec::from_iter(backends.iter().cloned()).into_boxed_slice();
        let mut weighted = Vec::with_capacity(backends.len());
        for (index, b) in backends.iter().enumerate() {
            for _ in 0..b.weight {
                weighted.push(index as u16);
            }
        }
        WeightedSelector {
            backends,
            weighted: weighted.into_boxed_slice(),
            algorithm: H::new(),
        }
    }
}

impl<H: SelectionAlgorithm + Send + Sync> BackendSelection for WeightedSelector<H> {
    type Iter = WeightedIterator<H>;

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        WeightedIterator::new(key, self.clone())
    }
}

/// An iterator over the backends of a [Weighted] selection.
///
/// See [super::BackendSelection] for more information.
pub struct WeightedIterator<H> {
    // the unbounded index seed
    index: u64,
    backend: Arc<WeightedSelector<H>>,
    first: bool,
}

impl<H: SelectionAlgorithm> WeightedIterator<H> {
    /// Constructs a new [WeightedIterator].
    fn new(input: &[u8], backend: Arc<WeightedSelector<H>>) -> Self {
        Self {
            index: backend.algorithm.next(input),
            backend,
            first: true,
        }
    }
}

impl<H: SelectionAlgorithm> BackendIter for WeightedIterator<H> {
    fn next(&mut self) -> Option<&Backend> {
        if self.backend.backends.is_empty() {
            // short circuit if empty
            return None;
        }

        if self.first {
            // initial hash, select from the weighted list
            self.first = false;
            let len = self.backend.weighted.len();
            let index = self.backend.weighted[self.index as usize % len];
            Some(&self.backend.backends[index as usize])
        } else {
            // fallback, select from the unique list
            // deterministically select the next item
            self.index = self.backend.algorithm.next(&self.index.to_le_bytes());
            let len = self.backend.backends.len();
            Some(&self.backend.backends[self.index as usize % len])
        }
    }
}

/// An iterator which wraps another iterator and yields unique items. It optionally takes a max
/// number of iterations if the wrapped iterator never returns.
pub struct UniqueIterator<I>
where
    I: BackendIter,
{
    iter: I,
    seen: HashSet<u64>,
    max_iterations: usize,
    steps: usize,
}

impl<I> UniqueIterator<I>
where
    I: BackendIter,
{
    /// Wrap a new iterator and specify the maximum number of times we want to iterate.
    pub fn new(iter: I, max_iterations: usize) -> Self {
        Self {
            iter,
            max_iterations,
            seen: HashSet::new(),
            steps: 0,
        }
    }

    pub fn get_next(&mut self) -> Option<Backend> {
        while let Some(item) = self.iter.next() {
            if self.steps >= self.max_iterations {
                return None;
            }
            self.steps += 1;

            let hash_key = item.hash_key();
            if !self.seen.contains(&hash_key) {
                self.seen.insert(hash_key);
                return Some(item.clone());
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestIter {
        seq: Vec<Backend>,
        idx: usize,
    }
    impl TestIter {
        fn new(input: &[&Backend]) -> Self {
            Self {
                seq: input.iter().cloned().cloned().collect(),
                idx: 0,
            }
        }
    }
    impl BackendIter for TestIter {
        fn next(&mut self) -> Option<&Backend> {
            let idx = self.idx;
            self.idx += 1;
            self.seq.get(idx)
        }
    }

    #[test]
    fn unique_iter_max_iterations_is_correct() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let b2 = Backend::new("1.0.0.1:80").unwrap();
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        let items = [&b1, &b2, &b3];

        let mut all = UniqueIterator::new(TestIter::new(&items), 3);
        assert_eq!(all.get_next(), Some(b1.clone()));
        assert_eq!(all.get_next(), Some(b2.clone()));
        assert_eq!(all.get_next(), Some(b3.clone()));
        assert_eq!(all.get_next(), None);

        let mut stop = UniqueIterator::new(TestIter::new(&items), 1);
        assert_eq!(stop.get_next(), Some(b1));
        assert_eq!(stop.get_next(), None);
    }

    #[test]
    fn unique_iter_duplicate_items_are_filtered() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let b2 = Backend::new("1.0.0.1:80").unwrap();
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        let items = [&b1, &b1, &b2, &b2, &b2, &b3];

        let mut uniq = UniqueIterator::new(TestIter::new(&items), 10);
        assert_eq!(uniq.get_next(), Some(b1));
        assert_eq!(uniq.get_next(), Some(b2));
        assert_eq!(uniq.get_next(), Some(b3));
    }
}
