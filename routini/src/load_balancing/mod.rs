use arc_swap::ArcSwap;
use futures::FutureExt;
use pingora::lb::Extensions;
use pingora::prelude::HttpPeer;
use pingora::protocols::l4::socket::SocketAddr;
use pingora::{ErrorType, OrErr, Result};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::io::Result as IoResult;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

mod background;
pub mod discovery;
pub mod health_check;
pub mod strategy;

use discovery::ServiceDiscovery;
use health_check::Health;
use strategy::BackendSelection;

use crate::load_balancing::strategy::Strategy;
use crate::load_balancing::strategy::utils::UniqueIterator;

/// Per-backend runtime metrics. Implementors carry shared state behind an `Arc` so that cloning a
/// [`Backend`] (which the selectors and `select()` do) keeps all clones pointing at the same
/// counters. The `Default` + `Clone` supertraits let [`Backends`] mint and preserve metrics
/// without dynamic dispatch — `Backend<M>` monomorphizes per concrete `M`.
pub trait Metrics: Clone + Default + Send + Sync + Debug + 'static {
    fn increment_active_connections(&self) {}
    fn decrement_active_connections(&self) {}
    fn record_latency(&self, _latency: Duration, _alpha: f32) {}
    fn active_connections(&self) -> Option<usize> {
        None
    }
    fn average_latency(&self) -> Option<f32> {
        None
    }
}

/// Zero-sized metrics implementation: the default `M` when a backend tracks nothing. Every method
/// is a no-op and compiles away entirely (no `Arc`, no `Option`, no branch).
#[derive(Clone, Copy, Default, Debug)]
pub struct NoMetric;

impl Metrics for NoMetric {}

/// [Backend] represents a server to proxy or connect to, generic over its metrics type `M`.
#[derive(Clone, Debug)]
pub struct Backend<M: Metrics = NoMetric> {
    /// The address to the backend server.
    pub addr: SocketAddr,
    /// The relative weight of the server. Load balancing algorithms will
    /// proportionally distributed traffic according to this value.
    pub weight: usize,

    /// The extension field to put arbitrary data to annotate the Backend.
    /// The data added here is opaque to this crate hence the data is ignored by
    /// functionalities of this crate. See [Extensions] for how to add and read the data.
    pub ext: Extensions,

    /// The upstream peer used to connect to this backend (default plain-HTTP to `addr`).
    pub peer: HttpPeer,

    /// Per-backend metrics. Identity (`Eq`/`Ord`/`Hash`) ignores this field.
    pub metrics: M,
}

// Identity is `(addr, weight)` only; `ext`, `peer` and `metrics` are intentionally excluded so two
// backends with the same address+weight are considered the same (and so `M` needs no Ord/Hash/Eq).
impl<M: Metrics> PartialEq for Backend<M> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && self.weight == other.weight
    }
}
impl<M: Metrics> Eq for Backend<M> {}
impl<M: Metrics> PartialOrd for Backend<M> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<M: Metrics> Ord for Backend<M> {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.addr, self.weight).cmp(&(&other.addr, other.weight))
    }
}
impl<M: Metrics> Hash for Backend<M> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.weight.hash(state);
    }
}

impl Backend<NoMetric> {
    /// Create a new [Backend] with `weight` 1 and [`NoMetric`].
    pub fn new(addr: &str) -> Result<Self> {
        Self::new_with_weight(addr, 1)
    }

    /// Create a new [Backend] with the specified `weight` and [`NoMetric`].
    pub fn new_with_weight(addr: &str, weight: usize) -> Result<Self> {
        Self::build(addr, weight)
    }
}

impl<M: Metrics> Backend<M> {
    /// Create a backend with the given weight and a default `M`. The peer defaults to plain HTTP
    /// to `addr`. The function will try to parse `addr` into a [std::net::SocketAddr].
    pub fn build(addr: &str, weight: usize) -> Result<Self> {
        let inet: std::net::SocketAddr = addr
            .parse()
            .or_err(ErrorType::InternalError, "invalid socket addr")?;
        let addr = SocketAddr::Inet(inet);
        let peer = HttpPeer::new(addr.clone(), false, String::new());
        Ok(Backend {
            addr,
            weight,
            ext: Extensions::new(),
            peer,
            metrics: M::default(),
        })
        // TODO: UDS
    }

    pub(crate) fn hash_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl<M: Metrics> std::ops::Deref for Backend<M> {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.addr
    }
}

impl<M: Metrics> std::ops::DerefMut for Backend<M> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.addr
    }
}

impl<M: Metrics> std::net::ToSocketAddrs for Backend<M> {
    type Iter = std::iter::Once<std::net::SocketAddr>;

    fn to_socket_addrs(&self) -> std::io::Result<Self::Iter> {
        self.addr.to_socket_addrs()
    }
}

/// [Backends] is a collection of [Backend]s.
///
/// It includes a service discovery method (static or dynamic) to discover all
/// the available backends as well as an optional health check method to probe the liveness
/// of each backend.
pub struct Backends<M: Metrics = NoMetric> {
    discovery: Box<dyn ServiceDiscovery<M> + Send + Sync + 'static>,
    health_check: Option<Arc<dyn health_check::HealthCheck<M> + Send + Sync + 'static>>,
    backends: ArcSwap<BTreeSet<Backend<M>>>,
    health: ArcSwap<HashMap<u64, Health>>,
}

impl<M: Metrics> Backends<M> {
    /// Create a new [Backends] with the given [ServiceDiscovery] implementation.
    ///
    /// The health check method is by default empty.
    pub fn new(discovery: Box<dyn ServiceDiscovery<M> + Send + Sync + 'static>) -> Self {
        Self {
            discovery,
            health_check: None,
            backends: Default::default(),
            health: Default::default(),
        }
    }

    /// Set the health check method. See [health_check] for the methods provided.
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck<M> + Send + Sync + 'static>,
    ) {
        self.health_check = Some(hc.into())
    }

    /// Updates backends when the new is different from the current set,
    /// the callback will be invoked when the new set of backend is different
    /// from the current one so that the caller can update the selector accordingly.
    fn do_update<F, S>(
        &self,
        _strategy: &S,
        new_backends: BTreeSet<Backend<M>>,
        enablement: HashMap<u64, bool>,
        callback: F,
    ) where
        F: Fn(Arc<BTreeSet<Backend<M>>>),
        S: Strategy<M>,
    {
        if (**self.backends.load()) != new_backends {
            let old_backends = self.backends.load();
            let mut backends = BTreeSet::new();

            let old_health = self.health.load();
            let mut health = HashMap::with_capacity(new_backends.len());

            for mut backend in new_backends.into_iter() {
                // Uses the old backend if it exists, to preserve extensions and metrics if any
                if let Some(old_backend) = old_backends.get(&backend) {
                    backend.ext.extend(old_backend.ext.clone());
                    backend.metrics = old_backend.metrics.clone();
                } else {
                    backend.metrics = M::default();
                }

                let hash_key = backend.hash_key();

                // use the default health if the backend is new
                let backend_health = old_health.get(&hash_key).cloned().unwrap_or_default();

                // override enablement
                if let Some(backend_enabled) = enablement.get(&hash_key) {
                    backend_health.enable(*backend_enabled);
                }
                health.insert(hash_key, backend_health);
                backends.insert(backend);
            }

            // TODO: put this all under 1 ArcSwap so the update is atomic
            // It's important the `callback()` executes first since computing selector backends might
            // be expensive. For example, if a caller checks `backends` to see if any are available
            // they may encounter false positives if the selector isn't ready yet.
            let backends = Arc::new(backends);
            callback(backends.clone());
            self.backends.store(backends);
            self.health.store(Arc::new(health));
        } else {
            // no backend change, just check enablement
            for (hash_key, backend_enabled) in enablement.iter() {
                // override enablement if set
                // this get should always be Some(_) because we already populate `health`` for all known backends
                if let Some(backend_health) = self.health.load().get(hash_key) {
                    backend_health.enable(*backend_enabled);
                }
            }
        }
    }

    /// Whether a certain [Backend] is ready to serve traffic.
    ///
    /// This function returns true when the backend is both healthy and enabled.
    /// This function returns true when the health check is unset but the backend is enabled.
    /// When the health check is set, this function will return false for the `backend` it
    /// doesn't know.
    pub fn ready(&self, backend: &Backend<M>) -> bool {
        self.health
            .load()
            .get(&backend.hash_key())
            // Racing: return `None` when this function is called between the
            // backend store and the health store
            .map_or(self.health_check.is_none(), |h| h.ready())
    }

    /// Manually set if a [Backend] is ready to serve traffic.
    ///
    /// This method does not override the health of the backend. It is meant to be used
    /// to stop a backend from accepting traffic when it is still healthy.
    ///
    /// This method is noop when the given backend doesn't exist in the service discovery.
    pub fn set_enable(&self, backend: &Backend<M>, enabled: bool) {
        // this should always be Some(_) because health is always populated during update
        if let Some(h) = self.health.load().get(&backend.hash_key()) {
            h.enable(enabled)
        };
    }

    /// Return the collection of the backends.
    pub fn get_backend(&self) -> Arc<BTreeSet<Backend<M>>> {
        self.backends.load_full()
    }

    /// Call the service discovery method to update the collection of backends.
    ///
    /// The callback will be invoked when the new set of backend is different
    /// from the current one so that the caller can update the selector accordingly.
    pub async fn update<F, S>(&self, strategy: &S, callback: F) -> Result<()>
    where
        F: Fn(Arc<BTreeSet<Backend<M>>>),
        S: Strategy<M>,
    {
        let (new_backends, enablement) = self.discovery.discover().await?;
        self.do_update(strategy, new_backends, enablement, callback);
        Ok(())
    }

    /// Run health check on all backends if it is set.
    ///
    /// When `parallel: true`, all backends are checked in parallel instead of sequentially
    pub async fn run_health_check(&self, parallel: bool) {
        use health_check::HealthCheck;
        use log::{info, warn};
        use pingora_runtime::current_handle;

        async fn check_and_report<M: Metrics>(
            backend: &Backend<M>,
            check: &Arc<dyn HealthCheck<M> + Send + Sync>,
            health_table: &HashMap<u64, Health>,
        ) {
            let errored = check.check(backend).await.err();
            if let Some(h) = health_table.get(&backend.hash_key()) {
                let flipped =
                    h.observe_health(errored.is_none(), check.health_threshold(errored.is_none()));
                if flipped {
                    check.health_status_change(backend, errored.is_none()).await;
                    let summary = check.backend_summary(backend);
                    if let Some(e) = errored {
                        warn!("{summary} becomes unhealthy, {e}");
                    } else {
                        info!("{summary} becomes healthy");
                    }
                }
            }
        }

        let Some(health_check) = self.health_check.as_ref() else {
            return;
        };

        let backends = self.backends.load();
        if parallel {
            let health_table = self.health.load_full();
            let runtime = current_handle();
            let jobs = backends.iter().map(|backend| {
                let backend = backend.clone();
                let check = health_check.clone();
                let ht = health_table.clone();
                runtime.spawn(async move {
                    check_and_report(&backend, &check, &ht).await;
                })
            });

            futures::future::join_all(jobs).await;
        } else {
            for backend in backends.iter() {
                check_and_report(backend, health_check, &self.health.load()).await;
            }
        }
    }
}

/// A [LoadBalancer] instance contains the service discovery, health check and backend selection
/// all together.
///
/// In order to run service discovery and health check at the designated frequencies, the [LoadBalancer]
/// needs to be run as a [pingora_core::services::background::BackgroundService].
pub struct LoadBalancer<S, M = NoMetric>
where
    S: Strategy<M>,
    M: Metrics,
{
    backends: Backends<M>,
    /// Controls what Strategy should be used when rebuilding the selector after an update.
    /// Usually a zero sized type that implements [Strategy]
    /// Uses RwLock to ensure there are no race conditions between the `update_strategy` and `update` methods.
    strategy: RwLock<S>,
    selector: ArcSwap<S::BackendSelector>,
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

impl<S, M> LoadBalancer<S, M>
where
    S: Strategy<M>,
    M: Metrics,
{
    /// Build a [LoadBalancer] with static backends created from the iter.
    ///
    /// Note: [ToSocketAddrs] will invoke blocking network IO for DNS lookup if
    /// the input cannot be directly parsed as [SocketAddr].
    pub fn try_from_iter_with_strategy<A, T: IntoIterator<Item = A>>(
        iter: T,
        strategy: S,
    ) -> IoResult<Self>
    where
        A: ToSocketAddrs,
    {
        let discovery = discovery::Static::try_from_iter(iter)?;
        let backends = Backends::new(discovery);
        let lb = Self::from_backends_with_strategy(backends, strategy);
        lb.update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");
        Ok(lb)
    }

    pub fn try_from_iter<A, T: IntoIterator<Item = A>>(iter: T) -> IoResult<Self>
    where
        A: ToSocketAddrs,
        S: Default,
    {
        Self::try_from_iter_with_strategy(iter, S::default())
    }

    /// Build a [LoadBalancer] with the given [Backends].
    pub fn from_backends(backends: Backends<M>) -> Self
    where
        S: Default,
    {
        Self::from_backends_with_strategy(backends, S::default())
    }

    pub fn from_backends_with_strategy(backends: Backends<M>, strategy: S) -> Self {
        // Backends already carry their `M` (constructed with `M::default()` during discovery), so
        // there's no metrics reset to do here — just build the initial selector.
        let selector = strategy.build_backend_selector(&backends.backends.load());
        LoadBalancer {
            backends,
            strategy: RwLock::new(strategy),
            selector: ArcSwap::new(Arc::new(selector)),
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
        let strategy = self.strategy.read().await;
        self.backends
            .update(&*strategy, |backends| {
                self.selector
                    .store(strategy.build_backend_selector(&backends).into());
            })
            .await
    }

    pub async fn rebuild_frequency(&self) -> Option<Duration> {
        self.strategy.read().await.rebuild_frequency()
    }

    pub async fn current_strategy(&self) -> S
    where
        S: Clone,
    {
        self.strategy.read().await.clone()
    }

    pub async fn rebuild_selector(&self) {
        let strategy = self.strategy.read().await;
        self.selector.store(
            strategy
                .build_backend_selector(&*self.backends.backends.load())
                .into(),
        );
    }

    /// Stores the new strategy and rebuilds the selector according to the new strategy.
    /// If this method is run on a load balancer with a static strategy, it will do nothing.
    pub async fn update_strategy(&self, strategy: S) -> bool
    where
        S: PartialEq + Display,
    {
        let mut current_strategy = self.strategy.write().await;
        if strategy == *current_strategy {
            return false;
        }

        log::info!("Updating strategy: {}", strategy);
        *current_strategy = strategy;
        self.selector.store(
            current_strategy
                .build_backend_selector(&*self.backends.backends.load())
                .into(),
        );
        true
    }

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
    pub fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend<M>> {
        self.select_with(key, max_iterations, |_, health| health)
    }

    /// Similar to [Self::select], return the first healthy [Backend] according to the selection algorithm
    /// and the user defined `accept` function.
    ///
    /// The `accept` function takes two inputs, the backend being selected and the internal health of that
    /// backend. The function can do things like ignoring the internal health checks or skipping this backend
    /// because it failed before. The `accept` function is called multiple times iterating over backends
    /// until it returns `true`.
    pub fn select_with<F>(&self, key: &[u8], max_iterations: usize, accept: F) -> Option<Backend<M>>
    where
        F: Fn(&Backend<M>, bool) -> bool,
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
        hc: Box<dyn health_check::HealthCheck<M> + Send + Sync + 'static>,
    ) {
        self.backends.set_health_check(hc);
    }

    /// Access the [Backends] of this [LoadBalancer]
    pub fn backends(&self) -> &Backends<M> {
        &self.backends
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    use crate::load_balancing::strategy::RoundRobin;

    use super::*;
    use async_trait::async_trait;
    use pingora::Result;

    #[tokio::test]
    async fn test_static_backends() {
        let backends: LoadBalancer<strategy::round_robin::RoundRobin> =
            LoadBalancer::try_from_iter(["1.1.1.1:80", "1.0.0.1:80"]).unwrap();

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
        let strategy = RoundRobin;

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(&strategy, |_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        // false: no new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(&strategy, |_| updated.store(true, Relaxed))
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
        let strategy = RoundRobin;

        // fill in the backends
        backends.update(&strategy, |_| {}).await.unwrap();

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
        let strategy = RoundRobin;

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(&strategy, |_| updated.store(true, Relaxed))
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
        let strategy = RoundRobin;

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(&strategy, |_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        backends.run_health_check(true).await;

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }

    mod thread_safety {
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
            let lb = Arc::new(LoadBalancer::<strategy::Consistent>::from_backends(
                Backends::new(Box::new(discovery)),
            ));
            let lb2 = lb.clone();

            tokio::spawn(async move {
                assert!(lb2.update().await.is_ok());
            });
            let mut backend_count = 0;
            while backend_count == 0 {
                let backends = lb.backends();
                backend_count = backends.backends.load_full().len();
            }
            assert_eq!(backend_count, expected);
            assert!(lb.select_with(b"test", 1, |_, _| true).is_some());
        }
    }
}
