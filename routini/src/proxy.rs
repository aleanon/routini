use http::StatusCode;
use matchit::Router;
use std::sync::Arc;

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
    upstreams::peer::Tracer,
};

use crate::{
    application::RouteConfig,
    load_balancing::{
        LoadBalancer,
        strategy::{Adaptive, fewest_connections::ConnectionsTracer},
    },
};

type MaxIterations = usize;

pub struct RouteValue {
    pub lb: Arc<LoadBalancer<Adaptive>>,
    pub max_iterations: MaxIterations,
    pub route_config: RouteConfig,
}

#[derive(Clone)]
pub struct Proxy {
    routes: Arc<Router<RouteValue>>,
}

impl Proxy {
    pub fn new(routes: Router<RouteValue>) -> Self {
        Proxy {
            routes: Arc::new(routes),
        }
    }

    pub fn route(&self, path: &str) -> Result<(&RouteValue, Option<String>)> {
        tracing::info!("route path: {}", path);
        self.routes
            .at(path)
            .map_err(|e| {
                Box::new(Error {
                    cause: Some(Box::new(e)),
                    context: Some(ImmutStr::Static("Failed to route path to backend")),
                    esource: ErrorSource::Internal,
                    etype: ErrorType::HTTPStatus(StatusCode::BAD_REQUEST.as_u16()),
                    retry: RetryType::Decided(false),
                })
            })
            .map(|m| {
                let value = m.value;
                let stripped_path = if value.route_config.strip_path_prefix {
                    m.params.get("rest").map(|p| p.to_string())
                } else {
                    None
                };
                (value, stripped_path)
            })
    }
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = Option<String>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let (route_value, new_path) = self.route(path)?;

        let backend = route_value
            .lb
            .select(&[], route_value.max_iterations)
            .ok_or(Error {
                context: Some(ImmutStr::Static("No healthy backends available")),
                cause: None,
                etype: ErrorType::InternalError,
                esource: ErrorSource::Internal,
                retry: RetryType::Decided(true),
            })?;

        if let Some(path) = new_path {
            session.req_header_mut().set_raw_path(path.as_bytes())?;
        }

        let tracer = Tracer(Box::new(ConnectionsTracer(backend.addr.clone())));
        let mut peer = Box::new(HttpPeer::new(backend, false, String::new()));
        peer.options.tracer = Some(tracer);
        Ok(peer)
    }
}
