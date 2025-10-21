use std::sync::Arc;

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
    upstreams::peer::Tracer,
};
use tracing::instrument;

use crate::load_balancing::{
    LoadBalancer,
    strategy::{BackendIter, BackendSelection, Strategy, fewest_connections::ConnectionsTracer},
};

pub const DEFAULT_MAX_ALGORITHM_ITERATIONS: usize = 256;

pub struct LB<S>
where
    S: Strategy,
    <S::BackendSelector as BackendSelection>::Iter: BackendIter,
{
    pub load_balancer: Arc<LoadBalancer<S>>,
}

#[async_trait::async_trait]
impl<S: Strategy> ProxyHttp for LB<S>
where
    S: Strategy,
    S::BackendSelector: BackendSelection + Send + Sync,
    <S::BackendSelector as BackendSelection>::Iter: BackendIter,
{
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    #[instrument(skip_all, err(Debug))]
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = self
            .load_balancer
            .select(&[], DEFAULT_MAX_ALGORITHM_ITERATIONS)
            .ok_or(Error {
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
