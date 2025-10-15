use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
    upstreams::peer::Tracer,
};
use tracing::instrument;

use crate::{
    application::StrategyKind,
    load_balancing::{
        Backend, LoadBalancer,
        selection::{BackendIter, BackendSelection, least_connections::ConnectionsTracer},
    },
};

// DEFAULT_MAX_ALGORITHM_ITERATIONS is there to bound the search time for the next Backend. In certain algorithm like Ketama hashing, the search for the next backend is linear and could take a lot of steps.
pub const DEFAULT_MAX_ALGORITHM_ITERATIONS: usize = 256;

pub type StrategyId = String;

/// Trait object wrapper over `LoadBalancer<S>` so we can store different algorithms together.
pub trait DynLoadBalancer: Send + Sync {
    fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend>;
}

impl<S> DynLoadBalancer for LoadBalancer<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend> {
        LoadBalancer::select(self, key, max_iterations)
    }
}

/// Routing configuration describing the active strategy.
#[derive(Clone)]
pub struct RoutingConfig {
    active: StrategyId,
}

impl RoutingConfig {
    pub fn new(active: StrategyKind) -> Self {
        Self {
            active: active.to_string(),
        }
    }

    pub fn strategy(&self) -> &str {
        &self.active
    }
}

pub struct MultiLoadBalancer {
    strategies: HashMap<StrategyId, Arc<dyn DynLoadBalancer>>,
    routing_rules: Arc<ArcSwap<RoutingConfig>>,
    max_iterations: usize,
}

#[derive(Clone)]
pub struct MultiLoadBalancerHandle {
    routing_rules: Arc<ArcSwap<RoutingConfig>>,
}

impl MultiLoadBalancerHandle {
    pub fn update_routing(&self, new_config: RoutingConfig) {
        self.routing_rules.store(Arc::new(new_config));
    }

    pub fn load(&self) -> Arc<RoutingConfig> {
        self.routing_rules.load_full()
    }
}

impl MultiLoadBalancer {
    pub fn new(
        strategies: HashMap<StrategyId, Arc<dyn DynLoadBalancer>>,
        initial_config: RoutingConfig,
        max_iterations: usize,
    ) -> Self {
        let default = initial_config.strategy().to_string();
        assert!(
            strategies.contains_key(&default),
            "initial routing config must reference an existing strategy"
        );

        Self {
            strategies,
            routing_rules: Arc::new(ArcSwap::new(Arc::new(initial_config))),
            max_iterations,
        }
    }

    pub fn handle(&self) -> MultiLoadBalancerHandle {
        MultiLoadBalancerHandle {
            routing_rules: Arc::clone(&self.routing_rules),
        }
    }
}

#[async_trait]
impl ProxyHttp for MultiLoadBalancer {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    #[instrument(skip_all, err(Debug))]
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let config = self.routing_rules.load();
        let strategy_id = config.strategy();
        let strategy = self.strategies.get(strategy_id).ok_or(Error {
            context: Some(ImmutStr::Static("Requested strategy not registered")),
            cause: None,
            etype: ErrorType::InternalError,
            esource: ErrorSource::Internal,
            retry: RetryType::Decided(true),
        })?;
        let upstream = strategy.select(&[], self.max_iterations).ok_or(Error {
            context: Some(ImmutStr::Static("No healthy backends available")),
            cause: None,
            etype: ErrorType::InternalError,
            esource: ErrorSource::Internal,
            retry: RetryType::Decided(true),
        })?;

        let tracer = Tracer(Box::new(ConnectionsTracer(upstream.addr.clone())));
        let mut peer = Box::new(HttpPeer::new(upstream, false, String::new()));
        peer.options.tracer = Some(tracer);
        Ok(peer)
    }
}
