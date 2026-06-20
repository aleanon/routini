use http::StatusCode;
use matchit::Router;
use quick_cache::sync::Cache;
use std::{net::IpAddr, sync::Arc, time::Instant};

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    http::{RequestHeader, ResponseHeader},
    prelude::HttpPeer,
    protocols::{Digest, l4::socket::SocketAddr},
    proxy::{ProxyHttp, Session},
};

use crate::{
    load_balancing::{Backend, Metrics},
    route::RouteRuntime,
    utils::constants::{DEFAULT_PATH_CACHE_CAPACITY, DEFAULT_PATH_REMAINDER_IDENTIFIER},
};

pub struct RouteValue {
    pub runtime: Arc<RouteRuntime>,
}

/// The pre-computed result of routing a concrete request path, stored in the path cache.
///
/// Both fields are cheap to clone: `runtime` is an `Arc` (the route table is immutable after build,
/// so the same config + balancer is shared) and `stripped_path` is computed once and reused,
/// removing the per-request `format!`/`to_string` allocation from the hot path.
pub struct CachedRoute {
    pub runtime: Arc<RouteRuntime>,
    /// The rewritten upstream path when `strip_path_prefix` is enabled, already encoded as bytes
    /// ready for `set_raw_path`. `None` when the path is forwarded unchanged.
    pub stripped_path: Option<Box<[u8]>>,
}

/// Extract the client IP from the downstream session, if it is an inet socket.
fn client_ip(session: &Session) -> Option<IpAddr> {
    match session.client_addr()? {
        SocketAddr::Inet(addr) => Some(addr.ip()),
        _ => None,
    }
}

/// Whether the downstream connection terminated TLS at this proxy.
fn client_is_tls(session: &Session) -> bool {
    session
        .digest()
        .map(|d| d.ssl_digest.is_some())
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct Proxy {
    routes: Arc<Router<RouteValue>>,
    /// Bounded concurrent cache mapping a concrete request path to its resolved route. The first
    /// request to a path does the matchit lookup and stripping; subsequent requests reuse the
    /// cached `Arc<CachedRoute>` with no allocation. Routes never change after build, so entries
    /// never need invalidation — eviction only bounds memory.
    cache: Arc<Cache<Box<str>, Arc<CachedRoute>>>,
}

impl Proxy {
    pub fn new(routes: Router<RouteValue>) -> Self {
        Self::with_cache_capacity(routes, DEFAULT_PATH_CACHE_CAPACITY)
    }

    pub fn with_cache_capacity(routes: Router<RouteValue>, capacity: usize) -> Self {
        Proxy {
            routes: Arc::new(routes),
            cache: Arc::new(Cache::new(capacity)),
        }
    }

    /// Resolve a concrete request path to its (cached) route.
    ///
    /// On a cache hit this is a single concurrent-map lookup plus an `Arc` clone. On a miss it
    /// performs the matchit lookup and computes the stripped path once, then caches the result.
    pub fn resolve(&self, path: &str) -> Result<Arc<CachedRoute>> {
        if let Some(cached) = self.cache.get(path) {
            return Ok(cached);
        }

        let (route_value, stripped_path) = self.route(path)?;
        let cached = Arc::new(CachedRoute {
            runtime: route_value.runtime.clone(),
            stripped_path: stripped_path.map(|p| p.into_bytes().into_boxed_slice()),
        });

        self.cache.insert(path.into(), cached.clone());
        Ok(cached)
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
                        .runtime
                        .config
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
    /// The route resolved for this request (set in `request_filter`), shared with later filters.
    route: Option<Arc<CachedRoute>>,
    upstream_start: Option<Instant>,
    backend: Option<Backend>,
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = ConnectionCTX;

    fn new_ctx(&self) -> Self::CTX {
        ConnectionCTX {
            route: None,
            upstream_start: None,
            backend: None,
        }
    }

    /// Resolve the route once, up front, so every later filter can read it from `ctx` and so
    /// short-circuit responses (redirects, limits) have a place to live. A path with no matching
    /// route is answered with 404 here rather than failing later in `upstream_peer`.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let resolved = {
            let path = session.req_header().uri.path();
            self.resolve(path)
        };

        match resolved {
            Ok(cached) => {
                ctx.route = Some(cached);
                Ok(false)
            }
            Err(_) => {
                session.respond_error(StatusCode::NOT_FOUND.as_u16()).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let route = ctx.route.clone().ok_or(Error {
            context: Some(ImmutStr::Static("Route not resolved")),
            cause: None,
            etype: ErrorType::InternalError,
            esource: ErrorSource::Internal,
            retry: RetryType::Decided(false),
        })?;

        let backend = route.runtime.lb.select(&[]).ok_or(Error {
            context: Some(ImmutStr::Static("No healthy backends available")),
            cause: None,
            etype: ErrorType::InternalError,
            esource: ErrorSource::Internal,
            retry: RetryType::Decided(true),
        })?;

        if let Some(stripped_path) = &route.stripped_path {
            session.req_header_mut().set_raw_path(stripped_path)?;
        }

        let mut peer = match backend.ext.get::<HttpPeer>() {
            Some(peer) => peer.clone(),
            None => {
                log::error!("HttpPeer not attached to backend: {}", &backend.addr);
                HttpPeer::new(backend.addr.clone(), false, String::new())
            }
        };

        route.runtime.config.timeouts.apply(&mut peer);

        ctx.backend = Some(backend);

        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.upstream_start = Some(Instant::now());

        if let Some(route) = &ctx.route {
            let ip = client_ip(session);
            let tls = client_is_tls(session);
            route
                .runtime
                .config
                .headers
                .apply_request(upstream_request, ip, tls);
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(route) = &ctx.route {
            route.runtime.config.headers.apply_response(upstream_response);
        }
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
            if let (Some(route), Some(backend)) = (&ctx.route, &ctx.backend) {
                backend
                    .metrics
                    .record_latency(latency, route.runtime.lb.config.latency_smoothing_factor);
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
        adaptive_loadbalancer::{
            AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine, options::AdaptiveLbOpt,
        },
        load_balancing::{Backends, discovery::Static},
        route::RouteConfig,
    };

    use super::*;

    fn create_test_route_value(strip_path: bool) -> RouteValue {
        let mut backends = BTreeSet::new();
        backends.insert(Backend::new("127.0.0.1:8080").unwrap());
        let backends = Backends::new(Static::new(backends));
        let decision_engine = AdaptiveDecisionEngine::new(&AdaptiveLbOpt::default());
        let lb = AdaptiveLoadBalancer::from_backends(backends, None, decision_engine);

        let config = RouteConfig {
            strip_path_prefix: strip_path,
            ..Default::default()
        };
        RouteValue {
            runtime: Arc::new(RouteRuntime::new(Arc::new(lb), config)),
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
        assert!(route_value.runtime.config.strip_path_prefix);
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
        assert!(route_value.runtime.config.strip_path_prefix);
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
        assert!(route_value.runtime.config.strip_path_prefix);
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
    fn test_resolve_caches_and_strips() {
        let mut router = Router::new();
        router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);

        let first = proxy.resolve("/auth/login").expect("should resolve");
        assert_eq!(first.stripped_path.as_deref(), Some(b"/login".as_slice()));

        // Second resolve of the same path must return the very same cached Arc.
        let second = proxy.resolve("/auth/login").expect("should resolve");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn test_resolve_no_strip_has_no_path() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let resolved = proxy.resolve("/api").expect("should resolve");
        assert!(resolved.stripped_path.is_none());
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
