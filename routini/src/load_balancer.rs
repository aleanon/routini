use std::sync::Arc;

use async_trait::async_trait;
use pingora::{
    Result,
    lb::LoadBalancer,
    prelude::{HttpPeer, RoundRobin},
    proxy::{ProxyHttp, Session},
};

pub struct LB {
    pub backends: Arc<LoadBalancer<RoundRobin>>,
}

#[async_trait]
impl ProxyHttp for LB {
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
