use std::sync::Arc;

use async_trait::async_trait;
use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    lb::{
        LoadBalancer,
        selection::{BackendIter, BackendSelection},
    },
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
};
use tracing::instrument;

const MAX_ALGORITHM_ITERATIONS: usize = 256;

pub struct LB<Algorithm> {
    pub backends: Arc<LoadBalancer<Algorithm>>,
}

#[async_trait]
impl<A> ProxyHttp for LB<A>
where
    A: BackendSelection + 'static + Send + Sync,
    A::Iter: BackendIter,
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
            .backends
            .select(&[], MAX_ALGORITHM_ITERATIONS)
            .ok_or(Error {
                context: Some(ImmutStr::Static("No healthy backends available")),
                cause: None,
                etype: ErrorType::InternalError,
                esource: ErrorSource::Internal,
                retry: RetryType::Decided(true),
            })?;

        let peer = Box::new(HttpPeer::new(upstream, false, "".to_string()));
        Ok(peer)
    }
}
