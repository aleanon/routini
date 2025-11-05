use http::StatusCode;
use matchit::Router;
use std::{sync::Arc, time::Instant};

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    http::{RequestHeader, ResponseHeader},
    prelude::HttpPeer,
    protocols::Digest,
    proxy::{ProxyHttp, Session},
};

use crate::{
    adaptive_loadbalancer::{AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine},
    load_balancing::{Backend, Metrics},
    server_builder::RouteConfig,
    utils::constants::DEFAULT_PATH_REMAINDER_IDENTIFIER,
};

pub struct RouteValue {
    pub lb: Arc<AdaptiveLoadBalancer<AdaptiveDecisionEngine>>,
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
        self.routes
            .at(path)
            .map_err(|e| {
                Box::new(Error {
                    cause: Some(Box::new(e)),
                    context: Some(ImmutStr::Static("Failed to route path to backend")),
                    esource: ErrorSource::Internal,
                    etype: ErrorType::HTTPStatus(StatusCode::NOT_FOUND.as_u16()),
                    retry: RetryType::Decided(false),
                })
            })
            .map(|m| {
                let value = m.value;
                let stripped_path =
                    value
                        .route_config
                        .strip_path_prefix
                        .then_some(m)
                        .and_then(|m| {
                            m.params.get(DEFAULT_PATH_REMAINDER_IDENTIFIER).map(|p| {
                                if !p.starts_with('/') {
                                    format!("/{p}")
                                } else {
                                    p.to_string()
                                }
                            })
                        });

                (value, stripped_path)
            })
    }
}

pub struct ConnectionCTX {
    upstream_start: Option<Instant>,
    lb: Option<Arc<AdaptiveLoadBalancer<AdaptiveDecisionEngine>>>,
    backend: Option<Backend>,
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = ConnectionCTX;

    fn new_ctx(&self) -> Self::CTX {
        ConnectionCTX {
            upstream_start: None,
            lb: None,
            backend: None,
        }
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let (route_value, stripped_path) = self.route(path)?;

        let backend = route_value.lb.select(&[]).ok_or(Error {
            context: Some(ImmutStr::Static("No healthy backends available")),
            cause: None,
            etype: ErrorType::InternalError,
            esource: ErrorSource::Internal,
            retry: RetryType::Decided(true),
        })?;

        if let Some(path) = stripped_path {
            session.req_header_mut().set_raw_path(path.as_bytes())?;
        }

        let peer = match backend.ext.get::<HttpPeer>() {
            Some(peer) => peer.clone(),
            None => {
                log::error!("HttpPeer not attached to backend: {}", &backend.addr);
                HttpPeer::new(backend.addr.clone(), false, String::new())
            }
        };

        ctx.backend = Some(backend);
        ctx.lb = Some(route_value.lb.clone());

        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        _upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.upstream_start = Some(Instant::now());
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(start) = ctx.upstream_start {
            let latency = start.elapsed();
            if let Some(lb) = &ctx.lb {
                if let Some(backend) = &ctx.backend {
                    backend
                        .metrics
                        .record_latency(latency, lb.config.latency_smoothing_factor);
                }
            }
        }
        Ok(())
    }
    /// This filter is called when the request just established or reused a connection to the upstream
    ///
    /// This filter allows user to log timing and connection related info.
    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(backend) = &ctx.backend {
            backend.metrics.increment_active_connections();
        }
        Ok(())
    }

    /// This filter is called when an HTTP stream/session to the upstream has ended
    ///
    /// This is called right before the connection is released back to the connection pool
    /// (for both HTTP/1 and HTTP/2). For HTTP/2, this is called once per stream, not per
    /// underlying connection.
    ///
    /// This allows tracking of concurrent request/stream counts per backend, latency
    /// measurements, or other per-request metrics.
    ///
    /// # Arguments
    /// * `session` - The downstream session
    /// * `peer` - The upstream peer that was connected to
    /// * `reused` - Whether the connection/stream was reused from the pool
    /// * `ctx` - The request context
    async fn upstream_stream_ended(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        _reused: bool,
        ctx: &mut Self::CTX,
    ) where
        Self::CTX: Send + Sync,
    {
        if let Some(backend) = &ctx.backend {
            backend.metrics.decrement_active_connections();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::{
        adaptive_loadbalancer::options::AdaptiveLbOpt,
        load_balancing::{Backends, discovery::Static},
    };

    use super::*;

    fn create_test_route_value(strip_path: bool) -> RouteValue {
        let mut backends = BTreeSet::new();
        backends.insert(Backend::new("127.0.0.1:8080").unwrap());
        let backends = Backends::new(Static::new(backends));
        let decision_engine = AdaptiveDecisionEngine::new(&AdaptiveLbOpt::default());
        let lb = AdaptiveLoadBalancer::from_backends(backends, None, decision_engine);

        RouteValue {
            lb: Arc::new(lb),
            route_config: RouteConfig {
                strip_path_prefix: strip_path,
            },
        }
    }

    #[test]
    fn test_route_basic_matching() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api");

        assert!(result.is_ok());
        let (_, stripped_path) = result.unwrap();
        assert_eq!(stripped_path, None);
    }

    #[test]
    fn test_route_with_wildcard() {
        let mut router = Router::new();
        router
            .insert("/api/{*rest}", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api/users/123");

        assert!(result.is_ok());
        let (_, stripped_path) = result.unwrap();
        assert_eq!(stripped_path, None);
    }

    #[test]
    fn test_route_with_path_stripping() {
        let mut router = Router::new();
        router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/auth/login");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped_path, Some("/login".to_string()));
    }

    #[test]
    fn test_route_with_path_stripping_nested() {
        let mut router = Router::new();
        router
            .insert("/api/v1/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api/v1/users/123/profile");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped_path, Some("/users/123/profile".to_string()));
    }

    #[test]
    fn test_route_not_found() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/nonexistent");

        assert!(result.is_err());
        if let Err(err) = result {
            assert_eq!(
                err.etype,
                ErrorType::HTTPStatus(StatusCode::NOT_FOUND.as_u16())
            );
        }
    }

    #[test]
    fn test_route_multiple_routes() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");
        router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);

        let result = proxy.route("/api");
        assert!(result.is_ok());
        let (_, stripped) = result.unwrap();
        assert_eq!(stripped, None);

        let result = proxy.route("/auth/logout");
        assert!(result.is_ok());
        let (route_value, stripped) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped, Some("/logout".to_string()));
    }

    #[test]
    fn test_route_exact_vs_wildcard_priority() {
        let mut router = Router::new();
        let exact_route = create_test_route_value(false);

        router
            .insert("/api/health", exact_route)
            .expect("Failed to insert exact route");
        router
            .insert("/api/{*rest}", create_test_route_value(false))
            .expect("Failed to insert wildcard route");

        let proxy = Proxy::new(router);

        let result = proxy.route("/api/health");
        assert!(result.is_ok());

        let result = proxy.route("/api/users");
        assert!(result.is_ok());
    }

    #[test]
    fn test_route_empty_path() {
        let mut router = Router::new();
        router
            .insert("/", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/");

        assert!(result.is_ok());
    }

    #[test]
    fn test_route_with_query_params() {
        let mut router = Router::new();
        router
            .insert("/search/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/search/query?q=test");

        assert!(result.is_ok());
        let (_, stripped_path) = result.unwrap();
        assert_eq!(stripped_path, Some("/query?q=test".to_string()));
    }
}
