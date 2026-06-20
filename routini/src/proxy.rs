use bytes::Bytes;
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

/// Parse the request's `Content-Length`, if present and valid.
fn content_length(session: &Session) -> Option<usize> {
    session
        .req_header()
        .headers
        .get(http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// A name-based virtual host: its own path router and path cache (nginx `server` block).
struct VHost {
    router: Router<RouteValue>,
    cache: Cache<Box<str>, Arc<CachedRoute>>,
}

#[derive(Clone)]
pub struct Proxy {
    /// Router + path cache for requests that match no configured virtual host (the default server).
    default_router: Arc<Router<RouteValue>>,
    /// Bounded concurrent cache mapping a concrete request path to its resolved route. The first
    /// request to a path does the matchit lookup and stripping; subsequent requests reuse the
    /// cached `Arc<CachedRoute>` with no allocation. Routes never change after build, so entries
    /// never need invalidation — eviction only bounds memory.
    default_cache: Arc<Cache<Box<str>, Arc<CachedRoute>>>,
    /// Name-based virtual hosts, keyed by lowercased host. Empty in the common single-host case,
    /// in which routing skips host handling entirely and behaves exactly like path-only routing.
    vhosts: Arc<std::collections::HashMap<String, VHost>>,
    /// Emit a structured access-log line per request (nginx `access_log on/off`).
    access_log: bool,
    /// Redirect plain-HTTP requests to `https://` (nginx `return 301 https://...`).
    https_redirect: bool,
}

impl Proxy {
    pub fn new(routes: Router<RouteValue>) -> Self {
        Self::with_cache_capacity(routes, DEFAULT_PATH_CACHE_CAPACITY)
    }

    pub fn with_cache_capacity(routes: Router<RouteValue>, capacity: usize) -> Self {
        Self::with_vhosts(routes, std::collections::HashMap::new(), capacity)
    }

    /// Build a proxy with a default router plus name-based virtual host routers.
    pub fn with_vhosts(
        default_router: Router<RouteValue>,
        vhosts: std::collections::HashMap<String, Router<RouteValue>>,
        capacity: usize,
    ) -> Self {
        let vhosts = vhosts
            .into_iter()
            .map(|(host, router)| {
                (
                    host,
                    VHost {
                        router,
                        cache: Cache::new(capacity),
                    },
                )
            })
            .collect();

        Proxy {
            default_router: Arc::new(default_router),
            default_cache: Arc::new(Cache::new(capacity)),
            vhosts: Arc::new(vhosts),
            access_log: true,
            https_redirect: false,
        }
    }

    /// Enable or disable the per-request access log.
    pub fn set_access_log(&mut self, enabled: bool) {
        self.access_log = enabled;
    }

    /// Redirect plain-HTTP requests to the `https://` equivalent.
    pub fn set_https_redirect(&mut self, enabled: bool) {
        self.https_redirect = enabled;
    }

    /// Resolve a request's `host` + `path` to its (cached) route.
    ///
    /// When virtual hosts are configured and the host matches one, that vhost's router/cache are
    /// used; otherwise the default server handles it. In the common (no-vhost) case this is a
    /// single concurrent-map lookup plus an `Arc` clone, identical to path-only routing.
    pub fn resolve(&self, host: Option<&str>, path: &str) -> Result<Arc<CachedRoute>> {
        if !self.vhosts.is_empty() {
            if let Some(host) = host {
                // Match nginx server_name semantics: case-insensitive, ignore the port.
                let key = host
                    .split(':')
                    .next()
                    .unwrap_or(host)
                    .to_ascii_lowercase();
                if let Some(vhost) = self.vhosts.get(&key) {
                    return Self::resolve_in(&vhost.router, &vhost.cache, path);
                }
            }
        }
        Self::resolve_in(&self.default_router, &self.default_cache, path)
    }

    fn resolve_in(
        router: &Router<RouteValue>,
        cache: &Cache<Box<str>, Arc<CachedRoute>>,
        path: &str,
    ) -> Result<Arc<CachedRoute>> {
        if let Some(cached) = cache.get(path) {
            return Ok(cached);
        }

        let (route_value, stripped_path) = Self::route_in(router, path)?;
        let cached = Arc::new(CachedRoute {
            runtime: route_value.runtime.clone(),
            stripped_path: stripped_path.map(|p| p.into_bytes().into_boxed_slice()),
        });

        cache.insert(path.into(), cached.clone());
        Ok(cached)
    }

    /// Look up a path in the default router (used by the strategy endpoint and tests).
    pub fn route(&self, path: &str) -> Result<(&RouteValue, Option<String>)> {
        Self::route_in(&self.default_router, path)
    }

    fn route_in<'r>(
        router: &'r Router<RouteValue>,
        path: &str,
    ) -> Result<(&'r RouteValue, Option<String>)> {
        router
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
    /// Backends already attempted this request, so retries pick a different one (failover).
    tried: Vec<SocketAddr>,
    /// Running total of request body bytes seen, for `max_body_size` enforcement.
    body_seen: usize,
    /// When the request was received, for access-log latency.
    request_start: Instant,
    /// Original request path (captured before prefix stripping) for the access log.
    orig_path: Option<Box<str>>,
    /// Held for the request's lifetime to keep the per-IP concurrency count accurate; the count is
    /// decremented when this guard drops at the end of the request.
    conn_guard: Option<pingora_limits::inflight::Guard>,
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = ConnectionCTX;

    fn new_ctx(&self) -> Self::CTX {
        ConnectionCTX {
            route: None,
            upstream_start: None,
            backend: None,
            tried: Vec::new(),
            body_seen: 0,
            request_start: Instant::now(),
            orig_path: None,
            conn_guard: None,
        }
    }

    /// Resolve the route once, up front, so every later filter can read it from `ctx` and so
    /// short-circuit responses (redirects, limits) have a place to live. A path with no matching
    /// route is answered with 404 here rather than failing later in `upstream_peer`.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // Redirect plain-HTTP requests to HTTPS before any routing work.
        if self.https_redirect && !client_is_tls(session) {
            let location = {
                let req = session.req_header();
                let host = req.uri.host().or_else(|| {
                    req.headers
                        .get(http::header::HOST)
                        .and_then(|h| h.to_str().ok())
                });
                host.map(|host| {
                    let pq = req
                        .uri
                        .path_and_query()
                        .map(|pq| pq.as_str())
                        .unwrap_or("/");
                    format!("https://{host}{pq}")
                })
            };

            if let Some(location) = location {
                let mut resp =
                    ResponseHeader::build(StatusCode::MOVED_PERMANENTLY.as_u16(), None)?;
                resp.insert_header(http::header::LOCATION, location)?;
                resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                session.write_response_header(Box::new(resp), true).await?;
                return Ok(true);
            }
        }

        let resolved = {
            let req = session.req_header();
            let path = req.uri.path();
            if self.access_log {
                ctx.orig_path = Some(Box::from(path));
            }
            // Prefer the URI authority (HTTP/2 :authority / absolute-form), fall back to Host header.
            let host = req.uri.host().or_else(|| {
                req.headers
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
            });
            self.resolve(host, path)
        };

        let cached = match resolved {
            Ok(cached) => cached,
            Err(_) => {
                session.respond_error(StatusCode::NOT_FOUND.as_u16()).await?;
                return Ok(true);
            }
        };

        // Per-client-IP rate and concurrency limits (nginx limit_req / limit_conn).
        let client = client_ip(session);
        if let (Some(limiter), Some(ip)) = (&cached.runtime.rate_limiter, client) {
            if limiter.over_limit(&ip) {
                session
                    .respond_error(StatusCode::TOO_MANY_REQUESTS.as_u16())
                    .await?;
                return Ok(true);
            }
        }
        if let (Some(limiter), Some(ip)) = (&cached.runtime.conn_limiter, client) {
            match limiter.acquire(&ip) {
                Ok(guard) => ctx.conn_guard = Some(guard),
                Err(()) => {
                    session
                        .respond_error(StatusCode::TOO_MANY_REQUESTS.as_u16())
                        .await?;
                    return Ok(true);
                }
            }
        }

        // Reject oversized bodies early via Content-Length (streaming bodies are guarded in
        // `request_body_filter`).
        if let Some(max) = cached.runtime.config.max_body_size {
            if content_length(session).is_some_and(|len| len > max) {
                session
                    .respond_error(StatusCode::PAYLOAD_TOO_LARGE.as_u16())
                    .await?;
                return Ok(true);
            }
        }

        ctx.route = Some(cached);
        Ok(false)
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let max = ctx.route.as_ref().and_then(|r| r.runtime.config.max_body_size);
        if let Some(max) = max {
            if let Some(chunk) = body.as_ref() {
                ctx.body_seen = ctx.body_seen.saturating_add(chunk.len());
                if ctx.body_seen > max {
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(StatusCode::PAYLOAD_TOO_LARGE.as_u16()),
                        "request body exceeds max_body_size",
                    ));
                }
            }
        }
        Ok(())
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

        // Restore any backends whose passive-health ejection window has elapsed.
        for backend in route.runtime.health.take_expired(Instant::now()) {
            route.runtime.lb.set_backend_enabled(&backend, true);
        }

        // Pick a healthy backend we have not already tried this request. A retry (driven by
        // `fail_to_connect`) re-enters here with the previous backend recorded, so failover lands
        // on a different upstream. Once exhausted the error is non-retryable.
        let backend = route
            .runtime
            .lb
            .select_excluding(&ctx.tried)
            .ok_or(Error {
                context: Some(ImmutStr::Static("No healthy backends available")),
                cause: None,
                etype: ErrorType::InternalError,
                esource: ErrorSource::Internal,
                retry: RetryType::Decided(false),
            })?;
        ctx.tried.push(backend.addr.clone());

        if let Some(stripped_path) = &route.stripped_path {
            session.req_header_mut().set_raw_path(stripped_path)?;
        }

        let tls = &route.runtime.config.upstream_tls;
        let mut peer = if tls.enabled {
            // Build a fresh HTTPS peer (sni + verification). Per-request cost is acceptable since
            // TLS upstreams are not the throughput-critical path; plain HTTP keeps the cached peer.
            let sni = tls.sni.clone().unwrap_or_default();
            let mut peer = HttpPeer::new(backend.addr.clone(), true, sni);
            peer.options.verify_cert = tls.verify;
            peer.options.verify_hostname = tls.verify;
            peer
        } else {
            match backend.ext.get::<HttpPeer>() {
                Some(peer) => peer.clone(),
                None => {
                    log::error!("HttpPeer not attached to backend: {}", &backend.addr);
                    HttpPeer::new(backend.addr.clone(), false, String::new())
                }
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
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(route) = &ctx.route {
            route.runtime.config.headers.apply_response(upstream_response);

            // HSTS is only meaningful over HTTPS, so add it only for TLS-terminated requests.
            if let Some(hsts) = &route.runtime.config.hsts {
                if client_is_tls(session) {
                    let _ = upstream_response
                        .insert_header(http::header::STRICT_TRANSPORT_SECURITY, hsts);
                }
            }
        }
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let (Some(route), Some(backend)) = (&ctx.route, &ctx.backend) {
            // A response means the connection succeeded: clear the passive-health failure window.
            route.runtime.health.record_success(backend);

            if let Some(start) = ctx.upstream_start {
                let latency = start.elapsed();
                backend
                    .metrics
                    .record_latency(latency, route.runtime.lb.config.latency_smoothing_factor);
            }
        }
        Ok(())
    }

    /// On a failed upstream *connection*, record a passive-health failure (ejecting the backend
    /// once it crosses `max_fails`) and mark the error retryable so `upstream_peer` is re-invoked
    /// to fail over to another backend, up to `max_retries`.
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        if let Some(route) = &ctx.route {
            if let Some(backend) = &ctx.backend {
                if route.runtime.health.record_failure(backend) {
                    route.runtime.lb.set_backend_enabled(backend, false);
                    log::warn!("Passively ejected backend {}", backend.addr);
                }
            }

            let retry = route.runtime.config.retry;
            if retry.retry_on_connect_error && ctx.tried.len() <= retry.max_retries {
                e.set_retry(true);
            }
        }
        e
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

    /// Emit a structured access-log record once the request completes.
    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        if !self.access_log {
            return;
        }

        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        let latency_ms = ctx.request_start.elapsed().as_millis();
        let bytes_sent = session.body_bytes_sent();
        let method = session.req_header().method.clone();
        let client = client_ip(session)
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        let upstream = ctx
            .backend
            .as_ref()
            .map(|b| b.addr.to_string())
            .unwrap_or_default();
        let path = ctx
            .orig_path
            .as_deref()
            .unwrap_or_else(|| session.req_header().uri.path());

        if let Some(err) = e {
            tracing::warn!(
                target: "routini::access",
                %client, %method, path, status, latency_ms, bytes_sent, upstream,
                error = %err,
                "request failed"
            );
        } else {
            tracing::info!(
                target: "routini::access",
                %client, %method, path, status, latency_ms, bytes_sent, upstream,
                "request"
            );
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

        let first = proxy.resolve(None, "/auth/login").expect("should resolve");
        assert_eq!(first.stripped_path.as_deref(), Some(b"/login".as_slice()));

        // Second resolve of the same path must return the very same cached Arc.
        let second = proxy.resolve(None, "/auth/login").expect("should resolve");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn test_vhost_routing_falls_back_to_default() {
        let mut default_router = Router::new();
        default_router
            .insert("/api", create_test_route_value(false))
            .unwrap();

        let mut vhost_router = Router::new();
        vhost_router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .unwrap();
        let mut vhosts = std::collections::HashMap::new();
        vhosts.insert("api.example.com".to_string(), vhost_router);

        let proxy = Proxy::with_vhosts(default_router, vhosts, 64);

        // Matching host uses the vhost router (case-insensitive, port ignored).
        let r = proxy
            .resolve(Some("API.example.com:8443"), "/auth/login")
            .expect("vhost route");
        assert_eq!(r.stripped_path.as_deref(), Some(b"/login".as_slice()));

        // The vhost path is not in the default router.
        assert!(proxy.resolve(None, "/auth/login").is_err());

        // Unknown host falls back to the default server.
        let r = proxy
            .resolve(Some("other.example.com"), "/api")
            .expect("default route");
        assert!(r.stripped_path.is_none());
    }

    #[test]
    fn test_resolve_no_strip_has_no_path() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let resolved = proxy.resolve(None, "/api").expect("should resolve");
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
