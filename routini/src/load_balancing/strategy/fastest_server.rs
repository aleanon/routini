use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use atomic_float::AtomicF32;

use crate::load_balancing::{
    Backend, BackendMetrics, Metrics,
    strategy::{BackendIter, BackendSelection, Strategy},
};

pub type LatencyEWMA = AtomicF32;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FastestServer;

impl Strategy for FastestServer {
    type BackendSelector = FastestServerSelector;

    fn metrics(&self) -> BackendMetrics {
        Some(Arc::new(LatencyEWMA::new(0.0)))
    }

    fn build_backend_selector(&self, backends: &BTreeSet<Backend>) -> Self::BackendSelector {
        let mut backends_with_metrics = Vec::with_capacity(backends.len());

        for backend in backends.iter() {
            let Some(latency) = backend.metrics.average_latency() else {
                log::error!("Missing Latency extension on backend");
                unreachable!("Implementation missing")
            };
            backends_with_metrics.push((backend, latency));
        }

        backends_with_metrics.sort_unstable_by(|a, b| a.1.total_cmp(&b.1));

        let backends = backends_with_metrics
            .into_iter()
            .map(|(b, _)| b.clone())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        FastestServerSelector { backends }
    }

    fn rebuild_frequency(&self) -> Option<Duration> {
        // TODO: Implement configurable value held in self
        Some(Duration::from_millis(200))
    }
}

pub struct FastestServerSelector {
    pub backends: Box<[Backend]>,
}

impl BackendSelection for FastestServerSelector {
    type Iter = FastestServerIter;

    fn iter(self: &std::sync::Arc<Self>, _key: &[u8]) -> Self::Iter {
        FastestServerIter {
            selector: self.clone(),
            index: 0,
        }
    }
}

pub struct FastestServerIter {
    selector: Arc<FastestServerSelector>,
    index: usize,
}

impl BackendIter for FastestServerIter {
    fn next(&mut self) -> Option<&Backend> {
        let backend = self.selector.backends.get(self.index)?;
        self.index += 1;
        Some(backend)
    }
}

impl Metrics for LatencyEWMA {
    fn record_latency(&self, latency: Duration, alpha: f32) {
        let old_avg = self.load(Ordering::Relaxed);
        let latency = latency.as_secs_f32() * 1000.0;

        let ewma = if old_avg == 0.0 {
            latency
        } else {
            let latency = alpha * latency + (1.0 - alpha) * old_avg;
            latency
        };

        self.store(ewma, Ordering::Relaxed);
    }

    fn average_latency(&self) -> Option<f32> {
        Some(self.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::constants::DEFAULT_SMOOTHING_FACTOR;

    use super::*;

    fn create_backend_with_latency(addr: &str) -> Backend {
        let mut backend = Backend::new(addr).unwrap();
        backend.metrics = Some(Arc::new(LatencyEWMA::new(0.0)));
        backend
    }

    fn set_backend_latency(backend: &Backend, latency_ms: Duration) {
        if let Some(metrics) = &backend.metrics {
            metrics.record_latency(latency_ms, DEFAULT_SMOOTHING_FACTOR);
        }
    }

    fn get_backend_latency(backend: &Backend) -> Option<f32> {
        backend.metrics.average_latency()
    }

    #[test]
    fn test_selection_order_by_latency() {
        // Create backends with latency metrics in their extensions
        let backend1 = create_backend_with_latency("127.0.0.1:8080");
        let backend2 = create_backend_with_latency("127.0.0.1:8081");
        let backend3 = create_backend_with_latency("127.0.0.1:8082");

        // Set initial latencies: backend1=50ms, backend2=10ms, backend3=100ms
        set_backend_latency(&backend1, Duration::from_millis(50));
        set_backend_latency(&backend2, Duration::from_millis(10));
        set_backend_latency(&backend3, Duration::from_millis(100));

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());
        backends.insert(backend3.clone());

        let strategy = FastestServer;
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        // First backend should be backend2 (10ms - fastest)
        let mut iter = selector.iter(&[]);
        let first = iter.next().expect("Should return first backend");
        assert_eq!(first.addr, backend2.addr);

        // Second should be backend1 (50ms)
        let second = iter.next().expect("Should return second backend");
        assert_eq!(second.addr, backend1.addr);

        // Third should be backend3 (100ms - slowest)
        let third = iter.next().expect("Should return third backend");
        assert_eq!(third.addr, backend3.addr);

        // No more backends
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_dynamic_latency_tracking() {
        // Create backends
        let backend1 = create_backend_with_latency("127.0.0.1:8080");
        let backend2 = create_backend_with_latency("127.0.0.1:8081");

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());

        let strategy = FastestServer;

        // Initial selection should pick backend1 (same latency, but comes first in BTreeSet)
        let selector = Arc::new(strategy.build_backend_selector(&backends));
        let mut iter = selector.iter(&[]);
        let selected = iter.next().unwrap();
        assert_eq!(selected.addr, backend1.addr);

        // Simulate backend1 becoming slower
        set_backend_latency(&backend1, Duration::from_millis(100));
        assert_eq!(get_backend_latency(&backend1), Some(100.0));
        assert_eq!(get_backend_latency(&backend2), Some(0.0));

        // Rebuild selector - now backend2 should be first (0ms vs 100ms)
        let selector = Arc::new(strategy.build_backend_selector(&backends));
        let mut iter = selector.iter(&[]);
        let selected = iter.next().unwrap();
        assert_eq!(selected.addr, backend2.addr);

        // Simulate backend2 also becoming slower
        set_backend_latency(&backend2, Duration::from_millis(200));

        // Rebuild selector - backend1 should be first now (100ms vs 200ms)
        let selector = Arc::new(strategy.build_backend_selector(&backends));
        let mut iter = selector.iter(&[]);
        let selected = iter.next().unwrap();
        assert_eq!(selected.addr, backend1.addr);
    }

    #[test]
    fn test_latency_updates() {
        let backend = create_backend_with_latency("127.0.0.1:8080");

        // Initial latency
        assert_eq!(get_backend_latency(&backend), Some(0.0));

        // First update should set it directly (since old_avg is 0.0)
        set_backend_latency(&backend, Duration::from_millis(100));
        assert_eq!(get_backend_latency(&backend), Some(100.0));

        // Second update will use EWMA: alpha * new + (1-alpha) * old
        // With DEFAULT_SMOOTHING_FACTOR, the result will be a weighted average
        set_backend_latency(&backend, Duration::from_millis(200));
        let latency = get_backend_latency(&backend).unwrap();
        // Should be between 100 and 200
        assert!(latency > 100.0 && latency < 200.0);

        // Third update continues the EWMA
        set_backend_latency(&backend, Duration::from_millis(50));
        let new_latency = get_backend_latency(&backend).unwrap();
        // Should be less than previous but not exactly 50
        assert!(new_latency < latency);
    }

    #[test]
    fn test_iter_exhaustion() {
        let backend1 = create_backend_with_latency("127.0.0.1:8080");
        let backend2 = create_backend_with_latency("127.0.0.1:8081");

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());

        let strategy = FastestServer;
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
        let backends = BTreeSet::new();
        let strategy = FastestServer;
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_single_backend() {
        let backend = create_backend_with_latency("127.0.0.1:8080");
        set_backend_latency(&backend, Duration::from_millis(123));

        let mut backends = BTreeSet::new();
        backends.insert(backend.clone());

        let strategy = FastestServer;
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);
        let selected = iter.next().expect("Should return the only backend");
        assert_eq!(selected.addr, backend.addr);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_equal_latencies() {
        // Create backends with identical latencies
        let backend1 = create_backend_with_latency("127.0.0.1:8080");
        let backend2 = create_backend_with_latency("127.0.0.1:8081");
        let backend3 = create_backend_with_latency("127.0.0.1:8082");

        // Set all to same latency
        set_backend_latency(&backend1, Duration::from_millis(50));
        set_backend_latency(&backend2, Duration::from_millis(50));
        set_backend_latency(&backend3, Duration::from_millis(50));

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());
        backends.insert(backend3.clone());

        let strategy = FastestServer;
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);

        // Should return all three backends (order will be stable based on BTreeSet ordering)
        assert!(iter.next().is_some());
        assert!(iter.next().is_some());
        assert!(iter.next().is_some());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_extreme_latencies() {
        let backend1 = create_backend_with_latency("127.0.0.1:8080");
        let backend2 = create_backend_with_latency("127.0.0.1:8081");
        let backend3 = create_backend_with_latency("127.0.0.1:8082");

        // Set extreme values
        set_backend_latency(&backend1, Duration::from_micros(1)); // Very fast (0.001ms)
        set_backend_latency(&backend2, Duration::from_secs(999)); // Very slow
        set_backend_latency(&backend3, Duration::from_millis(100)); // Normal

        let mut backends = BTreeSet::new();
        backends.insert(backend1.clone());
        backends.insert(backend2.clone());
        backends.insert(backend3.clone());

        let strategy = FastestServer;
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        let mut iter = selector.iter(&[]);

        // Should return in order: backend1 (fastest), backend3 (normal), backend2 (slowest)
        let first = iter.next().unwrap();
        assert_eq!(first.addr, backend1.addr);

        let second = iter.next().unwrap();
        assert_eq!(second.addr, backend3.addr);

        let third = iter.next().unwrap();
        assert_eq!(third.addr, backend2.addr);

        assert!(iter.next().is_none());
    }
}
