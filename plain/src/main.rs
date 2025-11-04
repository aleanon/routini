use std::{collections::BTreeSet, sync::Arc, thread, time::Duration};

use async_trait::async_trait;
use pingora::{
    Error, ErrorSource, ErrorType, Result, RetryType,
    lb::{Backend, Backends, LoadBalancer, discovery::Static},
    prelude::{HttpPeer, RoundRobin, TcpHealthCheck, background_service},
    proxy::{ProxyHttp, Session, http_proxy_service},
    server::{Server, configuration::ServerConf},
    upstreams,
};

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

struct Proxy {
    lb: Arc<LoadBalancer<RoundRobin>>,
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let backend = self.lb.select(&[], 256).ok_or(Error {
            cause: None,
            context: None,
            esource: ErrorSource::Internal,
            etype: ErrorType::InternalError,
            retry: RetryType::Decided(false),
        })?;

        let peer = backend.ext.get::<HttpPeer>().unwrap();
        Ok(Box::new(peer.clone()))
    }
}

fn main() {
    let mut server_config = ServerConf::default();
    server_config.upstream_keepalive_pool_size = 100000;
    let mut server = Server::new_with_opt_and_conf(None, server_config);

    let mut backends = BTreeSet::new();

    for i in 1..=40 {
        let mut backend = Backend::new(&format!("127.0.0.1:40{:02}", i)).unwrap();
        let http_peer = HttpPeer::new(backend.addr.clone(), false, String::new());
        backend.ext.insert(http_peer);
        backends.insert(backend);
    }
    let backends = Backends::new(Static::new(backends));

    let mut lb = LoadBalancer::<RoundRobin>::from_backends(backends);
    lb.set_health_check(TcpHealthCheck::new());
    lb.health_check_frequency = Some(Duration::from_secs(1));

    let mut bg_service = background_service("lb", lb);
    bg_service.threads = Some(1);
    let task = bg_service.task();

    let proxy = Proxy { lb: task };
    let mut proxy_service = http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp("127.0.0.1:3500");
    let available_thread = thread::available_parallelism().map(Into::into).unwrap_or(1);
    proxy_service.threads = Some(available_thread);

    server.add_service(bg_service);
    server.add_service(proxy_service);

    server.bootstrap();
    server.run_forever();
}
