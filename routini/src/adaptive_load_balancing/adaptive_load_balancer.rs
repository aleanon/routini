use std::{net::ToSocketAddrs, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use futures::FutureExt;
use pingora::Result;
use std::io::Result as IoResult;

use crate::load_balancing::{
    Backend, Backends, discovery, health_check,
    selection::{BackendIter, BackendSelection, UniqueIterator},
};

// pub trait StrategyBuilder {
//     type
// }

// pub struct StrategySelector<S, I> {
//     current_strategy:
//     current_strategy: ArcSwap<S>,
// }

/// A [LoadBalancer] instance contains the service discovery, health check and backend selection
/// all together.
///
/// In order to run service discovery and health check at the designated frequencies, the [LoadBalancer]
/// needs to be run as a [pingora_core::services::background::BackgroundService].
pub struct AdaptiveLoadBalancer<S> {
    backends: Backends,
    selector: ArcSwap<S>,
    /// How frequent the health check logic (if set) should run.
    ///
    /// If `None`, the health check logic will only run once at the beginning.
    pub health_check_frequency: Option<Duration>,
    /// How frequent the service discovery should run.
    ///
    /// If `None`, the service discovery will only run once at the beginning.
    pub update_frequency: Option<Duration>,
    /// Whether to run health check to all backends in parallel. Default is false.
    pub parallel_health_check: bool,
}

impl<S: BackendSelection> AdaptiveLoadBalancer<S>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    /// Build a [LoadBalancer] with static backends created from the iter.
    ///
    /// Note: [ToSocketAddrs] will invoke blocking network IO for DNS lookup if
    /// the input cannot be directly parsed as [SocketAddr].
    pub fn try_from_iter<A, T: IntoIterator<Item = A>>(iter: T) -> IoResult<Self>
    where
        A: ToSocketAddrs,
    {
        let discovery = discovery::Static::try_from_iter(iter)?;
        let backends = Backends::new(discovery);
        let lb = Self::from_backends(backends);
        lb.update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");
        Ok(lb)
    }

    /// Build a [LoadBalancer] with the given [Backends].
    pub fn from_backends(backends: Backends) -> Self {
        let selector = ArcSwap::new(Arc::new(S::build(&backends.get_backend())));
        AdaptiveLoadBalancer {
            backends,
            selector,
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }

    /// Run the service discovery and update the selection algorithm.
    ///
    /// This function will be called every `update_frequency` if this [LoadBalancer] instance
    /// is running as a background service.
    pub async fn update(&self) -> Result<()> {
        self.backends
            .update(|backends| self.selector.store(Arc::new(S::build(&backends))))
            .await
    }

    // pub async fn update(&self) -> Result<()> {
    //     self.backends
    //         .update(|backends| {
    //             let mut current = self.selector.load_full();
    //             let selector = Arc::make_mut(&mut current);
    //             selector.rebuild(&backends);
    //             self.selector.store(current);
    //         })
    //         .await
    // }

    /// Return the first healthy [Backend] according to the selection algorithm and the
    /// health check results.
    ///
    /// The `key` is used for hash based selection and is ignored if the selection is random or
    /// round robin.
    ///
    /// the `max_iterations` is there to bound the search time for the next Backend. In certain
    /// algorithm like Ketama hashing, the search for the next backend is linear and could take
    /// a lot steps.
    // TODO: consider remove `max_iterations` as users have no idea how to set it.
    pub fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend> {
        self.select_with(key, max_iterations, |_, health| health)
    }

    /// Similar to [Self::select], return the first healthy [Backend] according to the selection algorithm
    /// and the user defined `accept` function.
    ///
    /// The `accept` function takes two inputs, the backend being selected and the internal health of that
    /// backend. The function can do things like ignoring the internal health checks or skipping this backend
    /// because it failed before. The `accept` function is called multiple times iterating over backends
    /// until it returns `true`.
    pub fn select_with<F>(&self, key: &[u8], max_iterations: usize, accept: F) -> Option<Backend>
    where
        F: Fn(&Backend, bool) -> bool,
    {
        let selection = self.selector.load();
        let mut iter = UniqueIterator::new(selection.iter(key), max_iterations);
        while let Some(b) = iter.get_next() {
            if accept(&b, self.backends.ready(&b)) {
                return Some(b);
            }
        }
        None
    }

    /// Set the health check method. See [health_check].
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>,
    ) {
        self.backends.set_health_check(hc);
    }

    /// Access the [Backends] of this [LoadBalancer]
    pub fn backends(&self) -> &Backends {
        &self.backends
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::{BTreeSet, HashMap},
        sync::atomic::{AtomicBool, Ordering::Relaxed},
    };

    use crate::load_balancing::{discovery::ServiceDiscovery, selection::RoundRobin};

    use super::*;
    use async_trait::async_trait;
    use pingora::Result;

    #[tokio::test]
    async fn test_static_backends() {
        let backends: AdaptiveLoadBalancer<RoundRobin> =
            AdaptiveLoadBalancer::try_from_iter(["1.1.1.1:80", "1.0.0.1:80"]).unwrap();

        let backend1 = Backend::new("1.1.1.1:80").unwrap();
        let backend2 = Backend::new("1.0.0.1:80").unwrap();
        let backend = backends.backends().get_backend();
        assert!(backend.contains(&backend1));
        assert!(backend.contains(&backend2));
    }

    #[tokio::test]
    async fn test_backends() {
        let discovery = discovery::Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        // false: no new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(!updated.load(Relaxed));

        backends.run_health_check(false).await;

        let backend = backends.get_backend();
        assert!(backend.contains(&good1));
        assert!(backend.contains(&good2));
        assert!(backend.contains(&bad));

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }
    #[tokio::test]
    async fn test_backends_with_ext() {
        let discovery = discovery::Static::default();
        let mut b1 = Backend::new("1.1.1.1:80").unwrap();
        b1.ext.insert(true);
        let mut b2 = Backend::new("1.0.0.1:80").unwrap();
        b2.ext.insert(1u8);
        discovery.add(b1.clone());
        discovery.add(b2.clone());

        let backends = Backends::new(Box::new(discovery));

        // fill in the backends
        backends.update(|_| {}).await.unwrap();

        let backend = backends.get_backend();
        assert!(backend.contains(&b1));
        assert!(backend.contains(&b2));

        let b2 = backend.first().unwrap();
        assert_eq!(b2.ext.get::<u8>(), Some(&1));

        let b1 = backend.last().unwrap();
        assert_eq!(b1.ext.get::<bool>(), Some(&true));
    }

    #[tokio::test]
    async fn test_discovery_readiness() {
        use discovery::Static;

        struct TestDiscovery(Static);
        #[async_trait]
        impl ServiceDiscovery for TestDiscovery {
            async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
                let bad = Backend::new("127.0.0.1:79").unwrap();
                let (backends, mut readiness) = self.0.discover().await?;
                readiness.insert(bad.hash_key(), false);
                Ok((backends, readiness))
            }
        }
        let discovery = Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());
        let discovery = TestDiscovery(discovery);

        let backends = Backends::new(Box::new(discovery));

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        let backend = backends.get_backend();
        assert!(backend.contains(&good1));
        assert!(backend.contains(&good2));
        assert!(backend.contains(&bad));

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }

    #[tokio::test]
    async fn test_parallel_health_check() {
        let discovery = discovery::Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        backends.run_health_check(true).await;

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }

    mod thread_safety {

        use crate::load_balancing::selection;

        use super::*;

        struct MockDiscovery {
            expected: usize,
        }
        #[async_trait]
        impl ServiceDiscovery for MockDiscovery {
            async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
                let mut d = BTreeSet::new();
                let mut m = HashMap::with_capacity(self.expected);
                for i in 0..self.expected {
                    let b = Backend::new(&format!("1.1.1.1:{i}")).unwrap();
                    m.insert(i as u64, true);
                    d.insert(b);
                }
                Ok((d, m))
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn test_consistency() {
            let expected = 3000;
            let discovery = MockDiscovery { expected };
            let lb = Arc::new(
                AdaptiveLoadBalancer::<selection::Consistent>::from_backends(Backends::new(
                    Box::new(discovery),
                )),
            );
            let lb2 = lb.clone();

            tokio::spawn(async move {
                assert!(lb2.update().await.is_ok());
            });
            let mut backend_count = 0;
            while backend_count == 0 {
                let backends = lb.backends();
                backend_count = backends.get_backend().len();
            }
            assert_eq!(backend_count, expected);
            assert!(lb.select_with(b"test", 1, |_, _| true).is_some());
        }
    }
}
