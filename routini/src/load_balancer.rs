use std::sync::Arc;

use async_trait::async_trait;
use pingora::{
    Result,
    lb::{
        LoadBalancer,
        selection::{BackendIter, BackendSelection},
    },
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
};

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

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = self.backends.select(&[], 256).unwrap();

        let peer = Box::new(HttpPeer::new(upstream, false, "".to_string()));
        Ok(peer)
    }
}
