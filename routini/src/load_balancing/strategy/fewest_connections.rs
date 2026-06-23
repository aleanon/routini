use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::load_balancing::{
    Backend, Metrics, NoMetric,
    strategy::{BackendIter, BackendSelection, Strategy},
};

/// Active-connection counter shared across `Backend` clones via an internal `Arc`.
#[derive(Clone, Debug, Default)]
pub struct ActiveConnections(Arc<AtomicUsize>);

impl ActiveConnections {
    pub fn new(initial: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(initial)))
    }
}

impl Metrics for ActiveConnections {
    fn increment_active_connections(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn decrement_active_connections(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    fn active_connections(&self) -> Option<usize> {
        Some(self.0.load(Ordering::Relaxed))
    }
}

#[derive(Default, Clone, PartialEq)]
pub struct FewestConnections;

impl<M: Metrics> Strategy<M> for FewestConnections {
    type BackendSelector = FewestConnectionsSelector<M>;

    fn build_backend_selector(&self, backends: &BTreeSet<Backend<M>>) -> Self::BackendSelector {
        let mut backends_with_metrics = Vec::with_capacity(backends.len());

        for backend in backends.iter() {
            // Backends whose `M` doesn't track connections report 0 and sort as least-loaded.
            let nr_of_connections = backend.metrics.active_connections().unwrap_or(0);
            backends_with_metrics.push((backend, nr_of_connections));
        }

        backends_with_metrics.sort_unstable_by_key(|(_, nr_of_connections)| *nr_of_connections);

        let backends = backends_with_metrics
            .into_iter()
            .map(|(b, _)| b.clone())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        FewestConnectionsSelector { backends }
    }

    fn rebuild_frequency(&self) -> Option<Duration> {
        // TODO: Change this to a configurable value held in self
        Some(Duration::from_millis(200))
    }
}

pub struct FewestConnectionsSelector<M: Metrics = NoMetric> {
    backends: Box<[Backend<M>]>,
}

impl<M: Metrics> BackendSelection<M> for FewestConnectionsSelector<M> {
    type Iter = FewestConnectionsIter<M>;

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        FewestConnectionsIter::new(self.clone(), key)
    }
}

pub struct FewestConnectionsIter<M: Metrics = NoMetric> {
    least_connections: Arc<FewestConnectionsSelector<M>>,
    index: usize,
}

impl<M: Metrics> FewestConnectionsIter<M> {
    fn new(least_connections: Arc<FewestConnectionsSelector<M>>, _key: &[u8]) -> Self {
        Self {
            least_connections,
            index: 0,
        }
    }
}

impl<M: Metrics> BackendIter<M> for FewestConnectionsIter<M> {
    fn next(&mut self) -> Option<&Backend<M>> {
        let backend = self.least_connections.backends.get(self.index)?;
        self.index += 1;
        Some(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_backend_with_connections(addr: &str) -> Backend<ActiveConnections> {
        Backend::build(addr, 1).unwrap()
    }

    fn set_backend_connections(backend: &mut Backend<ActiveConnections>, count: usize) {
        backend.metrics = ActiveConnections::new(count);
    }

    fn increment_backend_connection(backend: &Backend<ActiveConnections>) {
        backend.metrics.increment_active_connections();
    }

    fn decrement_backend_connection(backend: &Backend<ActiveConnections>) {
        backend.metrics.decrement_active_connections();
    }

    fn get_backend_connections(backend: &Backend<ActiveConnections>) -> Option<usize> {
        backend.metrics.active_connections()
    }

    #[test]
    fn test_selection_order_by_connection_count() {
        // Create backends with atomic counters in their extensions
        let mut backend1 = create_backend_with_connections("127.0.0.1:8080");
        let mut backend2 = create_backend_with_connections("127.0.0.1:8081");
        let mut backend3 = create_backend_with_connections("127.0.0.1:8082");

        // Set initial connection counts: backend1=0, backend2=5, backend3=10
        set_backend_connections(&mut backend1, 0);
        set_backend_connections(&mut backend2, 5);
        set_backend_connections(&mut backend3, 10);

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());
        backends.insert(backend3.clone());

        let strategy = FewestConnections::default();
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        // First backend should be backend1 (0 connections)
        let mut iter = selector.iter(&[]);
        let first = iter.next().expect("Should return first backend");
        assert_eq!(first.addr, backend1.addr);

        // Second should be backend2 (5 connections)
        let second = iter.next().expect("Should return second backend");
        assert_eq!(second.addr, backend2.addr);

        // Third should be backend3 (10 connections)
        let third = iter.next().expect("Should return third backend");
        assert_eq!(third.addr, backend3.addr);

        // No more backends
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_dynamic_connection_tracking() {
        // Create backends
        let backend1 = create_backend_with_connections("127.0.0.1:8080");
        let backend2 = create_backend_with_connections("127.0.0.1:8081");

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());

        let strategy = FewestConnections::default();

        // Initial selection should pick backend1 (same count, but comes first in BTreeSet)
        let selector = Arc::new(strategy.build_backend_selector(&backends));
        let mut iter = selector.iter(&[]);
        let selected = iter.next().unwrap();
        assert_eq!(selected.addr, backend1.addr);

        // Simulate connection to backend1
        increment_backend_connection(&backend1);
        assert_eq!(get_backend_connections(&backend1), Some(1));
        assert_eq!(get_backend_connections(&backend2), Some(0));

        // Rebuild selector - now backend2 should be first (0 connections vs 1)
        let selector = Arc::new(strategy.build_backend_selector(&backends));
        let mut iter = selector.iter(&[]);
        let selected = iter.next().unwrap();
        assert_eq!(selected.addr, backend2.addr);

        // Simulate connection to backend2
        increment_backend_connection(&backend2);

        // Both now have 1 connection
        assert_eq!(get_backend_connections(&backend1), Some(1));
        assert_eq!(get_backend_connections(&backend2), Some(1));
    }

    #[test]
    fn test_connection_decrement() {
        let backend = create_backend_with_connections("127.0.0.1:8080");

        // Increment twice
        increment_backend_connection(&backend);
        increment_backend_connection(&backend);
        assert_eq!(get_backend_connections(&backend), Some(2));

        // Decrement once
        decrement_backend_connection(&backend);
        assert_eq!(get_backend_connections(&backend), Some(1));

        // Decrement again
        decrement_backend_connection(&backend);
        assert_eq!(get_backend_connections(&backend), Some(0));
    }

    #[test]
    fn test_iter_exhaustion() {
        let backend1 = create_backend_with_connections("127.0.0.1:8080");
        let backend2 = create_backend_with_connections("127.0.0.1:8081");

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());

        let strategy = FewestConnections::default();
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);

        // Should return both backends
        assert!(iter.next().is_some());
        assert!(iter.next().is_some());

        // Then None
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_empty_backends() {
        let backends: BTreeSet<Backend<ActiveConnections>> = BTreeSet::new();
        let strategy = FewestConnections::default();
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);
        assert!(iter.next().is_none());
    }
}
