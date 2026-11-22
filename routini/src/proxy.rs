use arc_swap::ArcSwap;
use bytes::Bytes;
use http::StatusCode;
use matchit::Router;
use pingora::cache::cache_control::CacheControl;
use pingora::cache::eviction::simple_lru::Manager as LruManager;
use pingora::cache::filters::resp_cacheable;
use pingora::cache::{CacheMeta, CacheMetaDefaults, MemCache, RespCacheable};
use quick_cache::sync::Cache;
use regex::Regex;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use std::{net::IpAddr, sync::Arc, time::Instant};

/// Shared in-memory response cache backend and its LRU eviction manager (nginx `proxy_cache_path`).
static CACHE_BACKEND: LazyLock<MemCache> = LazyLock::new(MemCache::new);
static CACHE_EVICTION: LazyLock<LruManager> = LazyLock::new(|| LruManager::new(256 * 1024 * 1024)); // 256 MiB
/// Only origin `Cache-Control` drives default cacheability; per-route TTL is applied explicitly.
const CACHE_DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(|_| None, 0, 0);

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    http::{RequestHeader, ResponseHeader},
    prelude::HttpPeer,
    protocols::{Digest, l4::socket::SocketAddr},
    proxy::{FailToProxy, ProxyHttp, Session},
};
use std::collections::HashMap;

use crate::{
    adaptive_loadbalancer::{AdaptiveBackend, options::HashKey},
    load_balancing::Metrics,
    metrics,
    rate_limit::Decision,
    real_ip::RealIp,
    route::{RouteAction, RouteRuntime, RouteState},
    utils::constants::{
        DEFAULT_CLIENT_BODY_TIMEOUT, DEFAULT_KEEPALIVE_TIMEOUT_SECS, DEFAULT_PATH_CACHE_CAPACITY,
        DEFAULT_PATH_REMAINDER_IDENTIFIER, DEFAULT_SEND_TIMEOUT,
    },
};

#[derive(Clone)]
pub struct RouteValue {
    pub runtime: Arc<RouteRuntime>,
}

/// The pre-computed result of routing a concrete request path, stored in the path cache.
///
/// Both fields are cheap to clone: `runtime` is an `Arc` (a [RoutingTable] is immutable, a config
/// reload swaps in a new table with fresh caches) and `stripped_path` is computed once and reused,
/// removing the per-request `format!`/`to_string` allocation from the hot path.
pub struct CachedRoute {
    pub runtime: Arc<RouteRuntime>,
    /// The rewritten upstream path when `strip_path_prefix` is enabled, already encoded as bytes
    /// ready for `set_raw_path`. `None` when the path is forwarded unchanged.
    pub stripped_path: Option<Box<[u8]>>,
}

/// The IP of the downstream peer (the client, or a proxy in front of us), if it is an inet
/// socket. See [RealIp] for the client behind trusted proxies.
fn client_ip(session: &Session) -> Option<IpAddr> {
    match session.client_addr()? {
        SocketAddr::Inet(addr) => Some(addr.ip().to_canonical()),
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

/// Expand a redirect `Location` template, substituting `$request_uri` (path + query) and `$uri`
/// (path). Returns the template unchanged when it contains no `$` placeholder.
fn redirect_location(template: &str, session: &Session) -> String {
    if !template.contains('$') {
        return template.to_string();
    }
    let uri = &session.req_header().uri;
    let path = uri.path();
    let request_uri = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(path);
    template
        .replace("$request_uri", request_uri)
        .replace("$uri", path)
}

/// `https://` location for a plain-HTTP request to `host` (Host header or URI authority) and
/// `path_and_query`. The request's port belongs to the plain listener, so it is replaced with
/// the TLS listener's port (omitted when 443). The host is parsed as an authority, which drops
/// userinfo and rejects anything that isn't `host[:port]`. `None` if the host is missing or
/// malformed.
fn https_location(
    host: Option<&str>,
    path_and_query: Option<&str>,
    https_port: u16,
) -> Option<String> {
    let authority: http::uri::Authority = host?.trim().parse().ok()?;
    let host = authority.host();
    if host.is_empty() {
        return None;
    }
    let port = match https_port {
        443 => String::new(),
        port => format!(":{port}"),
    };
    let path = path_and_query.filter(|p| p.starts_with('/')).unwrap_or("/");
    Some(format!("https://{host}{port}{path}"))
}

/// The bytes the hash strategies select a backend with, see [HashKey]. Borrowed from the
/// request where possible; the client address is written to `ip_buf`. A missing header or
/// cookie falls back to the client address, so such requests still spread across backends.
fn hash_key<'a>(
    key: &HashKey,
    req: &'a RequestHeader,
    client: Option<IpAddr>,
    ip_buf: &'a mut [u8; 16],
) -> &'a [u8] {
    let found = match key {
        HashKey::ClientIp => None,
        HashKey::Uri => Some(
            req.uri
                .path_and_query()
                .map_or(req.uri.path(), |pq| pq.as_str())
                .as_bytes(),
        ),
        HashKey::Path => Some(req.uri.path().as_bytes()),
        HashKey::Header(name) => req.headers.get(name).map(|v| v.as_bytes()),
        HashKey::Cookie(name) => cookie(req, name).map(str::as_bytes),
    };
    if let Some(found) = found {
        return found;
    }
    match client {
        Some(IpAddr::V4(ip)) => {
            ip_buf[..4].copy_from_slice(&ip.octets());
            &ip_buf[..4]
        }
        Some(IpAddr::V6(ip)) => {
            *ip_buf = ip.octets();
            &ip_buf[..]
        }
        None => &[],
    }
}

/// The value of cookie `name`, from any of the request's `Cookie` headers.
fn cookie<'a>(req: &'a RequestHeader, name: &str) -> Option<&'a str> {
    req.headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value.trim_matches('"'))
}

/// The virtual host serving `host` (a Host header or URI authority), with nginx `server_name`
/// semantics: case-insensitive, port ignored, an exact name before wildcards, and of the
/// wildcards the longest: for `a.b.example.com`, `*.b.example.com` before `*.example.com`. A
/// wildcard matches any depth of subdomain but not the bare domain.
fn find_vhost<'t>(vhosts: &'t HashMap<String, VHost>, host: &str) -> Option<&'t VHost> {
    let name = match host.strip_prefix('[') {
        // IPv6 literal, `[::1]:8080`
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if let Some(vhost) = vhosts.get(&name) {
        return Some(vhost);
    }
    let mut wildcard = String::with_capacity(name.len() + 1);
    let mut rest = name.as_str();
    while let Some((_, parent)) = rest.split_once('.') {
        wildcard.clear();
        wildcard.push_str("*.");
        wildcard.push_str(parent);
        if let Some(vhost) = vhosts.get(&wildcard) {
            return Some(vhost);
        }
        rest = parent;
    }
    None
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

/// Everything route lookup needs: routers, regex locations and their path caches. Immutable once
/// built; a config reload builds a new table and swaps it in whole ([Proxy::replace_routes]), so
/// cached lookups (including the pre-computed stripped path) never outlive the config they were
/// computed from.
pub struct RoutingTable {
    /// Router + path cache for requests that match no configured virtual host (the default server).
    default_router: Router<RouteValue>,
    /// Bounded concurrent cache mapping a concrete request path to its resolved route. The first
    /// request to a path does the matchit lookup and stripping; subsequent requests reuse the
    /// cached `Arc<CachedRoute>` with no allocation. Eviction only bounds memory.
    default_cache: Cache<Box<str>, Arc<CachedRoute>>,
    /// Name-based virtual hosts, keyed by lowercased host or `*.suffix` wildcard. Empty in the
    /// common single-host case, in which routing skips host handling entirely and behaves
    /// exactly like path-only routing.
    vhosts: HashMap<String, VHost>,
    /// Regex location routes for the default server, tried in order when matchit finds no match
    /// (nginx `location ~ <regex>`). These forward the full path (no prefix stripping).
    default_regex: Vec<(Regex, RouteValue)>,
    /// Every route in the table, for metrics.
    routes: Vec<Arc<RouteRuntime>>,
}

impl RoutingTable {
    /// Build a table from default matchit routes, default regex routes, and vhost routers.
    pub fn new(
        default_router: Router<RouteValue>,
        default_regex: Vec<(Regex, RouteValue)>,
        vhosts: HashMap<String, Router<RouteValue>>,
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
        Self {
            default_router,
            default_cache: Cache::new(capacity),
            vhosts,
            default_regex,
            routes: Vec::new(),
        }
    }

    /// List the table's routes, for metrics.
    pub fn with_routes(mut self, routes: Vec<Arc<RouteRuntime>>) -> Self {
        self.routes = routes;
        self
    }

    pub fn routes(&self) -> &[Arc<RouteRuntime>] {
        &self.routes
    }
}

#[derive(Clone)]
pub struct Proxy {
    /// Shared by every clone (the proxy service and the route manager), so a reload that swaps
    /// the table is seen by all of them.
    table: Arc<ArcSwap<RoutingTable>>,
    /// Emit a structured access-log line per request (nginx `access_log on/off`).
    access_log: bool,
    /// Redirect plain-HTTP requests to `https://` (nginx `return 308 https://...`).
    https_redirect: bool,
    /// Port of the TLS listener that redirects point to.
    https_port: u16,
    /// Downstream response compression level (nginx `gzip`); 0 disables it.
    compression_level: u32,
    /// Generate/propagate an `X-Request-Id` header for request tracing.
    request_id: bool,
    /// Custom error-page bodies keyed by status code (nginx `error_page`).
    error_pages: Arc<HashMap<u16, String>>,
    /// Where to find the client address behind trusted proxies. `None`: the peer address.
    real_ip: Option<Arc<RealIp>>,
    client_timeouts: ClientTimeouts,
}

/// Timeouts on client connections (nginx `keepalive_timeout`, `client_body_timeout`,
/// `send_timeout`). The header of a connection's first request always has pingora's 60s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientTimeouts {
    /// Idle seconds before a kept-alive HTTP/1 connection is closed, 0 disables keepalive.
    pub keepalive_secs: u64,
    /// Longest wait for more of the request body (HTTP/1).
    pub body: Duration,
    /// Longest wait for the client to take more of the response.
    pub send: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            keepalive_secs: DEFAULT_KEEPALIVE_TIMEOUT_SECS,
            body: DEFAULT_CLIENT_BODY_TIMEOUT,
            send: DEFAULT_SEND_TIMEOUT,
        }
    }
}

/// Map a fatal proxy error to the HTTP status to return (0 = downstream gone, do not respond).
fn error_to_status(e: &Error) -> u16 {
    match e.etype() {
        ErrorType::HTTPStatus(code) => *code,
        _ => match e.esource() {
            ErrorSource::Upstream => 502,
            ErrorSource::Downstream => 0,
            _ => 500,
        },
    }
}

/// Generate a random 64-bit request id as 16 lowercase hex chars.
fn generate_request_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

const X_REQUEST_ID: &str = "x-request-id";

impl Proxy {
    pub fn new(routes: Router<RouteValue>) -> Self {
        Self::with_cache_capacity(routes, DEFAULT_PATH_CACHE_CAPACITY)
    }

    pub fn with_cache_capacity(routes: Router<RouteValue>, capacity: usize) -> Self {
        Self::with_vhosts(routes, HashMap::new(), capacity)
    }

    /// Build a proxy with a default router plus name-based virtual host routers.
    pub fn with_vhosts(
        default_router: Router<RouteValue>,
        vhosts: HashMap<String, Router<RouteValue>>,
        capacity: usize,
    ) -> Self {
        Self::with_regex_routes(default_router, Vec::new(), vhosts, capacity)
    }

    /// Build a proxy with default matchit routes, default regex routes, and vhost routers.
    pub fn with_regex_routes(
        default_router: Router<RouteValue>,
        default_regex: Vec<(Regex, RouteValue)>,
        vhosts: HashMap<String, Router<RouteValue>>,
        capacity: usize,
    ) -> Self {
        Self::with_table(RoutingTable::new(
            default_router,
            default_regex,
            vhosts,
            capacity,
        ))
    }

    pub fn with_table(table: RoutingTable) -> Self {
        let table = Arc::new(ArcSwap::from_pointee(table));
        crate::metrics::watch(&table);
        Proxy {
            table,
            access_log: true,
            https_redirect: false,
            https_port: 443,
            compression_level: 0,
            request_id: false,
            error_pages: Arc::new(HashMap::new()),
            real_ip: None,
            client_timeouts: ClientTimeouts::default(),
        }
    }

    pub fn set_client_timeouts(&mut self, timeouts: ClientTimeouts) {
        self.client_timeouts = timeouts;
    }

    /// Take the client address from a header when the peer is a trusted proxy.
    pub fn set_real_ip(&mut self, real_ip: Option<RealIp>) {
        self.real_ip = real_ip.map(Arc::new);
    }

    /// Atomically swap the routing table. Requests already routed keep their resolved route.
    pub fn replace_routes(&self, table: RoutingTable) {
        self.table.store(Arc::new(table));
    }

    /// Set custom error-page bodies keyed by status code.
    pub fn set_error_pages(&mut self, pages: HashMap<u16, String>) {
        self.error_pages = Arc::new(pages);
    }

    /// Write a status response, using a configured custom error page when one exists.
    async fn write_status(&self, session: &mut Session, code: u16) -> Result<()> {
        match self.error_pages.get(&code) {
            Some(body) => {
                let bytes = Bytes::copy_from_slice(body.as_bytes());
                let mut resp = ResponseHeader::build(code, None)?;
                resp.insert_header(http::header::CONTENT_LENGTH, bytes.len().to_string())?;
                resp.insert_header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")?;
                session.write_response_header(Box::new(resp), false).await?;
                session.write_response_body(Some(bytes), true).await?;
                Ok(())
            }
            None => session.respond_error(code).await,
        }
    }

    /// Enable or disable the per-request access log.
    pub fn set_access_log(&mut self, enabled: bool) {
        self.access_log = enabled;
    }

    /// Set the downstream response compression level (0 disables).
    pub fn set_compression_level(&mut self, level: u32) {
        self.compression_level = level;
    }

    /// Enable `X-Request-Id` generation/propagation.
    pub fn set_request_id(&mut self, enabled: bool) {
        self.request_id = enabled;
    }

    /// Redirect plain-HTTP requests to the `https://` equivalent.
    pub fn set_https_redirect(&mut self, enabled: bool) {
        self.https_redirect = enabled;
    }

    /// Port of the TLS listener redirects point to. Default: 443.
    pub fn set_https_port(&mut self, port: u16) {
        self.https_port = port;
    }

    /// Resolve a request's `host` + `path` to its (cached) route.
    ///
    /// When virtual hosts are configured and the host matches one, that vhost's router/cache are
    /// used; otherwise the default server handles it. In the common (no-vhost) case this is a
    /// single concurrent-map lookup plus an `Arc` clone, identical to path-only routing.
    pub fn resolve(&self, host: Option<&str>, path: &str) -> Result<Arc<CachedRoute>> {
        let table = self.table.load();
        if !table.vhosts.is_empty()
            && let Some(vhost) = host.and_then(|host| find_vhost(&table.vhosts, host))
        {
            return Self::resolve_in(&vhost.router, &[], &vhost.cache, path);
        }
        Self::resolve_in(
            &table.default_router,
            &table.default_regex,
            &table.default_cache,
            path,
        )
    }

    fn resolve_in(
        router: &Router<RouteValue>,
        regex: &[(Regex, RouteValue)],
        cache: &Cache<Box<str>, Arc<CachedRoute>>,
        path: &str,
    ) -> Result<Arc<CachedRoute>> {
        if let Some(cached) = cache.get(path) {
            return Ok(cached);
        }

        let cached = match Self::route_in(router, path) {
            Ok((route_value, stripped_path)) => Arc::new(CachedRoute {
                runtime: route_value.runtime.clone(),
                stripped_path: stripped_path.map(|p| p.into_bytes().into_boxed_slice()),
            }),
            // matchit miss: fall back to regex locations (first match wins, full path forwarded).
            Err(not_found) => match regex.iter().find(|(re, _)| re.is_match(path)) {
                Some((_, route_value)) => Arc::new(CachedRoute {
                    runtime: route_value.runtime.clone(),
                    stripped_path: None,
                }),
                None => return Err(not_found),
            },
        };

        cache.insert(path.into(), cached.clone());
        Ok(cached)
    }

    /// Look up a path in the default router (used by tests).
    pub fn route(&self, path: &str) -> Result<(RouteValue, Option<String>)> {
        let table = self.table.load();
        Self::route_in(&table.default_router, path)
            .map(|(value, stripped)| (value.clone(), stripped))
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
                let strip = value.runtime.state.load().config.strip_path_prefix;
                let stripped_path = strip.then_some(m).and_then(|m| {
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
    /// Snapshot of the route's hot-swappable state, loaded once in `request_filter` so every later
    /// filter sees a consistent config for this request even if a reload happens mid-flight.
    state: Option<Arc<RouteState>>,
    upstream_start: Option<Instant>,
    backend: Option<AdaptiveBackend>,
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
    /// The request's `X-Request-Id` (incoming or generated) for upstream/response/log propagation.
    request_id: Option<String>,
    /// The client address (behind trusted proxies, see [RealIp]), resolved in `request_filter`.
    client_ip: Option<IpAddr>,
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = ConnectionCTX;

    fn new_ctx(&self) -> Self::CTX {
        ConnectionCTX {
            route: None,
            state: None,
            upstream_start: None,
            backend: None,
            tried: Vec::new(),
            body_seen: 0,
            request_start: Instant::now(),
            orig_path: None,
            conn_guard: None,
            request_id: None,
            client_ip: None,
        }
    }

    /// Apply the client timeouts before anything can answer the request.
    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let t = self.client_timeouts;
        // Only shorten keepalive: a client that sent `Connection: close` (or HTTP/1.0 without
        // keep-alive) has it off, and must not get it back
        if session.get_keepalive().is_some() {
            session.set_keepalive((t.keepalive_secs > 0).then_some(t.keepalive_secs));
        }
        session.set_read_timeout(Some(t.body));
        session.set_write_timeout(Some(t.send));
        Ok(())
    }

    /// Install the downstream response-compression module at the configured level (0 = disabled).
    fn init_downstream_modules(&self, modules: &mut pingora::modules::http::HttpModules) {
        modules.add_module(
            pingora::modules::http::compression::ResponseCompressionBuilder::enable(
                self.compression_level,
            ),
        );
    }

    /// Resolve the route once, up front, so every later filter can read it from `ctx` and so
    /// short-circuit responses (redirects, limits) have a place to live. A path with no matching
    /// route is answered with 404 here rather than failing later in `upstream_peer`.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // The client: the peer, or the address a trusted proxy forwarded
        let peer = client_ip(session);
        ctx.client_ip = match (&self.real_ip, peer) {
            (Some(real_ip), Some(peer)) => {
                match real_ip.resolve(peer, &session.req_header().headers) {
                    Some(client) => Some(client),
                    None => {
                        // A trusted proxy sent a chain that can't be read. Guessing the client
                        // would let it pass IP rules or share the proxy's own rate limit
                        // bucket, which its health checks depend on
                        self.write_status(session, StatusCode::BAD_REQUEST.as_u16())
                            .await?;
                        return Ok(true);
                    }
                }
            }
            _ => peer,
        };

        // Redirect plain-HTTP requests to HTTPS before any routing work.
        if self.https_redirect && !client_is_tls(session) {
            let location = {
                let req = session.req_header();
                let host = req.uri.authority().map(|a| a.as_str()).or_else(|| {
                    req.headers
                        .get(http::header::HOST)
                        .and_then(|h| h.to_str().ok())
                });
                let pq = req.uri.path_and_query().map(|pq| pq.as_str());
                https_location(host, pq, self.https_port)
            };

            if let Some(location) = location {
                // 308 rather than 301 so clients repeat POST/PUT bodies instead of switching
                // to GET
                let mut resp =
                    ResponseHeader::build(StatusCode::PERMANENT_REDIRECT.as_u16(), None)?;
                resp.insert_header(http::header::LOCATION, location)?;
                resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                session.write_response_header(Box::new(resp), true).await?;
                return Ok(true);
            }
        }

        // Establish a request id (reuse an incoming one, else generate) for tracing propagation.
        if self.request_id {
            let id = session
                .req_header()
                .headers
                .get(X_REQUEST_ID)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_else(generate_request_id);
            ctx.request_id = Some(id);
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
                self.write_status(session, StatusCode::NOT_FOUND.as_u16())
                    .await?;
                return Ok(true);
            }
        };

        // Snapshot the route's hot-swappable state once; every later filter reuses this via ctx.
        let state = cached.runtime.state.load_full();

        // Access control: IP allow/deny then HTTP Basic auth.
        if let Some(access) = &state.config.access {
            if let Some(ip) = ctx.client_ip {
                if !access.ip_allowed(ip) {
                    self.write_status(session, StatusCode::FORBIDDEN.as_u16())
                        .await?;
                    return Ok(true);
                }
            } else if !access.allow.is_empty() || !access.deny.is_empty() {
                // IP rules are set but the client address is unknown: deny rather than guess.
                self.write_status(session, StatusCode::FORBIDDEN.as_u16())
                    .await?;
                return Ok(true);
            }

            if access.requires_auth() {
                let auth = session
                    .req_header()
                    .headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|h| h.to_str().ok());
                let authorized = access.auth_ok(auth).await;
                if !authorized {
                    let realm = access.basic_auth_realm.as_deref().unwrap_or("Restricted");
                    let mut resp = ResponseHeader::build(StatusCode::UNAUTHORIZED.as_u16(), None)?;
                    resp.insert_header(
                        http::header::WWW_AUTHENTICATE,
                        format!("Basic realm=\"{realm}\""),
                    )?;
                    resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                    session.write_response_header(Box::new(resp), true).await?;
                    return Ok(true);
                }
            }
        }

        // Redirect / return actions short-circuit without proxying.
        if let Some(action) = &state.config.action {
            match action {
                RouteAction::Redirect { status, location } => {
                    let location = redirect_location(location, session);
                    let mut resp = ResponseHeader::build(*status, None)?;
                    resp.insert_header(http::header::LOCATION, location)?;
                    resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                    session.write_response_header(Box::new(resp), true).await?;
                }
                RouteAction::Return {
                    status,
                    body: Some(body),
                } => {
                    let bytes = Bytes::copy_from_slice(body.as_bytes());
                    let mut resp = ResponseHeader::build(*status, None)?;
                    resp.insert_header(http::header::CONTENT_LENGTH, bytes.len().to_string())?;
                    session.write_response_header(Box::new(resp), false).await?;
                    session.write_response_body(Some(bytes), true).await?;
                }
                RouteAction::Return { status, body: None } => {
                    let mut resp = ResponseHeader::build(*status, None)?;
                    resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                    session.write_response_header(Box::new(resp), true).await?;
                }
            }
            return Ok(true);
        }

        // Per-client-IP rate and concurrency limits (nginx limit_req / limit_conn).
        let client = ctx.client_ip;
        if let (Some(limiter), Some(ip)) = (&state.rate_limiter, client) {
            match limiter.check(ip, Instant::now()) {
                Decision::Allow => {}
                Decision::Delay(wait) => tokio::time::sleep(wait).await,
                Decision::Reject => {
                    self.write_status(session, StatusCode::TOO_MANY_REQUESTS.as_u16())
                        .await?;
                    return Ok(true);
                }
            }
        }
        if let (Some(limiter), Some(ip)) = (&state.conn_limiter, client) {
            match limiter.acquire(&ip) {
                Ok(guard) => ctx.conn_guard = Some(guard),
                Err(()) => {
                    self.write_status(session, StatusCode::TOO_MANY_REQUESTS.as_u16())
                        .await?;
                    return Ok(true);
                }
            }
        }

        // Reject oversized bodies early via Content-Length (streaming bodies are guarded in
        // `request_body_filter`).
        if let Some(max) = state.config.max_body_size {
            if content_length(session).is_some_and(|len| len > max) {
                self.write_status(session, StatusCode::PAYLOAD_TOO_LARGE.as_u16())
                    .await?;
                return Ok(true);
            }
        }

        ctx.state = Some(state);
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
        let max = ctx.state.as_ref().and_then(|s| s.config.max_body_size);
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

    /// Enable the shared response cache for cacheable (GET/HEAD) requests on cache-enabled routes.
    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<()> {
        let enabled = ctx
            .state
            .as_ref()
            .and_then(|s| s.config.cache.as_ref())
            .is_some_and(|c| c.enabled);
        if !enabled {
            return Ok(());
        }

        let method = &session.req_header().method;
        if *method != http::Method::GET && *method != http::Method::HEAD {
            return Ok(());
        }

        session.cache.enable(
            &*CACHE_BACKEND,
            Some(
                &*CACHE_EVICTION as &'static (dyn pingora::cache::eviction::EvictionManager + Sync),
            ),
            None,
            None,
            None,
        );
        Ok(())
    }

    /// Decide cacheability of an upstream response: honor origin `Cache-Control`, otherwise cache
    /// 200 responses for the route's configured TTL.
    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        let cc = CacheControl::from_resp_headers(resp);
        let cacheable = resp_cacheable(cc.as_ref(), resp.clone(), false, &CACHE_DEFAULTS);
        if matches!(cacheable, RespCacheable::Cacheable(_)) {
            return Ok(cacheable);
        }

        if resp.status == StatusCode::OK {
            if let Some(ttl) = ctx
                .state
                .as_ref()
                .and_then(|s| s.config.cache.as_ref())
                .map(|c| c.ttl)
            {
                let now = SystemTime::now();
                return Ok(RespCacheable::Cacheable(CacheMeta::new(
                    now + ttl,
                    now,
                    0,
                    0,
                    resp.clone(),
                )));
            }
        }
        Ok(cacheable)
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
        let state = ctx.state.clone();

        // Restore any backends whose passive-health ejection window has elapsed.
        if let Some(state) = &state {
            for backend in state.health.take_expired(Instant::now()) {
                route.runtime.lb.set_backend_enabled(&backend, true);
            }
        }

        // Pick a healthy backend we have not already tried this request. A retry (driven by
        // `fail_to_connect`) re-enters here with the previous backend recorded, so failover lands
        // on a different upstream. Once exhausted the error is non-retryable.
        let lb = &route.runtime.lb;
        let backend = {
            let mut ip_buf = [0; 16];
            let key = hash_key(
                &lb.config().hash_key,
                session.req_header(),
                ctx.client_ip,
                &mut ip_buf,
            );
            lb.select_excluding(key, &ctx.tried)
        };
        // All backends tried (e.g. a single one whose pooled connection had gone stale): retry on
        // any healthy one rather than failing a request that may well succeed
        let backend = backend.or_else(|| {
            let mut ip_buf = [0; 16];
            let key = hash_key(
                &lb.config().hash_key,
                session.req_header(),
                ctx.client_ip,
                &mut ip_buf,
            );
            (!ctx.tried.is_empty())
                .then(|| lb.select_excluding(key, &[]))
                .flatten()
        });
        let backend = backend.ok_or(Error {
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

        let tls = state.as_ref().map(|s| &s.config.upstream_tls);
        let mut peer = if tls.is_some_and(|t| t.enabled) {
            let tls = tls.unwrap();
            // Build a fresh HTTPS peer (sni + verification). Per-request cost is acceptable since
            // TLS upstreams are not the throughput-critical path; plain HTTP keeps the cached peer.
            let sni = tls.sni.clone().unwrap_or_default();
            let mut peer = HttpPeer::new(backend.addr.clone(), true, sni)?;
            peer.options.verify_cert = tls.verify;
            peer.options.verify_hostname = tls.verify;
            peer.options.ca = tls.ca.clone();
            peer.group_key = tls.ca_pool_key;
            peer.client_cert_key = tls.client_cert.clone();
            peer
        } else {
            // The backend carries its own (plain-HTTP) peer as a typed field.
            backend.peer.clone()
        };

        if tls.is_some_and(|t| t.h2) {
            // Prefer HTTP/2 (fall back to HTTP/1.1); over TLS this advertises h2 via ALPN.
            peer.options.set_http_version(2, 1);
        }

        if let Some(state) = &state {
            state.config.timeouts.apply(&mut peer);
        }

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

        if let Some(state) = &ctx.state {
            let peer = client_ip(session);
            let tls = client_is_tls(session);
            state
                .config
                .headers
                .apply_request(upstream_request, ctx.client_ip, peer, tls);
        }
        if let Some(id) = &ctx.request_id {
            let _ = upstream_request.insert_header(X_REQUEST_ID, id);
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(state) = &ctx.state {
            state.config.headers.apply_response(upstream_response);

            // HSTS is only meaningful over HTTPS, so add it only for TLS-terminated requests.
            if let Some(hsts) = &state.config.hsts {
                if client_is_tls(session) {
                    let _ = upstream_response
                        .insert_header(http::header::STRICT_TRANSPORT_SECURITY, hsts);
                }
            }
        }
        if let Some(id) = &ctx.request_id {
            let _ = upstream_response.insert_header(X_REQUEST_ID, id);
        }
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let (Some(route), Some(state), Some(backend)) = (&ctx.route, &ctx.state, &ctx.backend) {
            // A status the route retries on: fail this attempt before anything reaches the
            // client, so the request goes to the next backend (nginx `proxy_next_upstream
            // http_503`). The last allowed attempt's response is passed on as it is.
            let status = upstream_response.status.as_u16();
            let retry = &state.config.retry;
            if retry.retry_on_status.contains(&status)
                && ctx.tried.len() <= retry.max_retries
                && retry.may_resend(&session.req_header().method)
                && !session.as_ref().retry_buffer_truncated()
            {
                metrics::upstream_failure(&route.runtime.name, &backend.addr.to_string(), "status");
                if state.health.record_failure(backend) {
                    route.runtime.lb.set_backend_enabled(backend, false);
                    log::warn!("Passively ejected backend {}", backend.addr);
                }
                let mut e = Error::explain(
                    ErrorType::HTTPStatus(status),
                    "upstream status configured for retry",
                );
                e.set_retry(true);
                return Err(e);
            }

            // A response means the connection succeeded: clear the passive-health failure window.
            state.health.record_success(backend);

            if let Some(start) = ctx.upstream_start {
                let latency = start.elapsed();
                backend
                    .metrics
                    .record_latency(latency, route.runtime.lb.config().latency_smoothing_factor);
                metrics::UPSTREAM_RESPONSE
                    .with_label_values(&[&*route.runtime.name, &backend.addr.to_string()])
                    .observe(latency.as_secs_f64());
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
        if let (Some(route), Some(state)) = (&ctx.route, &ctx.state) {
            if let Some(backend) = &ctx.backend {
                metrics::upstream_failure(
                    &route.runtime.name,
                    &backend.addr.to_string(),
                    "connect",
                );
                if state.health.record_failure(backend) {
                    route.runtime.lb.set_backend_enabled(backend, false);
                    log::warn!("Passively ejected backend {}", backend.addr);
                }
            }

            let retry = &state.config.retry;
            if retry.retry_on_connect_error && ctx.tried.len() <= retry.max_retries {
                e.set_retry(true);
            }
        }
        e
    }

    /// An error after the request reached a backend. Pingora retries a failure on a reused
    /// (possibly stale) pooled connection; a timeout is retried too when the route allows it, see
    /// [RetryConfig]. Nothing is retried once the client has seen part of the response.
    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        let mut e = e.more_context(format!("Peer: {peer}"));
        // The backend's failures (a status retry is counted where it is decided)
        if *e.esource() == ErrorSource::Upstream
            && !matches!(e.etype(), ErrorType::HTTPStatus(_))
            && let (Some(route), Some(backend)) = (&ctx.route, &ctx.backend)
        {
            let kind = match e.etype() {
                ErrorType::ReadTimedout | ErrorType::WriteTimedout => "timeout",
                _ => "error",
            };
            metrics::upstream_failure(&route.runtime.name, &backend.addr.to_string(), kind);
        }
        let replayable =
            session.response_written().is_none() && !session.as_ref().retry_buffer_truncated();
        if !replayable {
            e.set_retry(false);
            return e;
        }
        // Retries set by upstream_response_filter (status) stand; a stale pooled connection is
        // retried as pingora does
        e.retry.decide_reuse(client_reused);

        // Only the backend's timeouts: a slow client (e.g. the client body timeout) is not the
        // backend's failure and must not get it ejected or the request resent
        let timed_out = *e.esource() == ErrorSource::Upstream
            && matches!(
                e.etype(),
                ErrorType::ReadTimedout | ErrorType::WriteTimedout
            );
        if let (true, Some(route), Some(state)) = (timed_out, &ctx.route, &ctx.state) {
            if let Some(backend) = &ctx.backend {
                if state.health.record_failure(backend) {
                    route.runtime.lb.set_backend_enabled(backend, false);
                    log::warn!("Passively ejected backend {}", backend.addr);
                }
            }
            let retry = &state.config.retry;
            if retry.retry_on_timeout
                && ctx.tried.len() <= retry.max_retries
                && retry.may_resend(&session.req_header().method)
            {
                e.set_retry(true);
            }
        }
        e
    }

    /// Serve a custom error page (when configured) for fatal proxy errors, e.g. no healthy backend.
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy
    where
        Self::CTX: Send + Sync,
    {
        let code = error_to_status(e);
        if code > 0 {
            if let Err(err) = self.write_status(session, code).await {
                log::error!("failed to send error response to downstream: {err}");
            }
        }
        FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
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
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        let route = ctx
            .route
            .as_ref()
            .map_or(metrics::NO_ROUTE, |r| &*r.runtime.name);
        let code = StatusCode::from_u16(status).ok();
        let status_label = code.as_ref().map_or("0", StatusCode::as_str);
        metrics::REQUESTS
            .with_label_values(&[route, status_label])
            .inc();
        metrics::REQUEST_DURATION
            .with_label_values(&[route])
            .observe(ctx.request_start.elapsed().as_secs_f64());

        if !self.access_log {
            return;
        }
        let latency_ms = ctx.request_start.elapsed().as_millis();
        let bytes_sent = session.body_bytes_sent();
        let method = session.req_header().method.clone();
        // Requests answered before the client was resolved log the peer
        let client = ctx
            .client_ip
            .or_else(|| client_ip(session))
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
        let request_id = ctx.request_id.as_deref().unwrap_or("");

        if let Some(err) = e {
            tracing::warn!(
                target: "routini::access",
                %client, %method, path, status, latency_ms, bytes_sent, upstream, request_id,
                error = %err,
                "request failed"
            );
        } else {
            tracing::info!(
                target: "routini::access",
                %client, %method, path, status, latency_ms, bytes_sent, upstream, request_id,
                "request"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    #[test]
    fn hash_key_picks_the_configured_part_of_the_request() {
        let mut req = RequestHeader::build("GET", b"/a/b?x=1", None).unwrap();
        req.append_header("cookie", "theme=dark; session=\"abc\"")
            .unwrap();
        req.append_header("cookie", "late=1").unwrap();
        req.insert_header("x-user", "u42").unwrap();
        let v4: Option<IpAddr> = "198.51.100.7".parse().ok();
        let v6: Option<IpAddr> = "2001:db8::1".parse().ok();
        let mut buf = [0; 16];

        assert_eq!(hash_key(&HashKey::Uri, &req, v4, &mut buf), b"/a/b?x=1");
        assert_eq!(hash_key(&HashKey::Path, &req, v4, &mut buf), b"/a/b");
        let header = HashKey::Header(http::HeaderName::from_static("x-user"));
        assert_eq!(hash_key(&header, &req, v4, &mut buf), b"u42");
        let session = HashKey::Cookie("session".into());
        assert_eq!(hash_key(&session, &req, v4, &mut buf), b"abc");
        let late = HashKey::Cookie("late".into());
        assert_eq!(hash_key(&late, &req, v4, &mut buf), b"1");

        assert_eq!(
            hash_key(&HashKey::ClientIp, &req, v4, &mut buf),
            &[198, 51, 100, 7]
        );
        assert_eq!(hash_key(&HashKey::ClientIp, &req, v6, &mut buf).len(), 16);
        // Missing header or cookie: the client address
        let missing = HashKey::Cookie("nope".into());
        assert_eq!(hash_key(&missing, &req, v4, &mut buf), &[198, 51, 100, 7]);
        assert_eq!(hash_key(&HashKey::ClientIp, &req, None, &mut buf), b"");
    }

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
        backends.insert(AdaptiveBackend::build("127.0.0.1:8080", 1).unwrap());
        let backends = Backends::new(Static::new(backends));
        let decision_engine = AdaptiveDecisionEngine::new(&AdaptiveLbOpt::default());
        let lb = AdaptiveLoadBalancer::from_backends(backends, None, decision_engine);

        let config = RouteConfig {
            strip_path_prefix: strip_path,
            ..Default::default()
        };
        RouteValue {
            runtime: Arc::new(RouteRuntime::new("/test", Arc::new(lb), config)),
        }
    }

    fn location(port: u16, host: Option<&str>, path: Option<&str>) -> Option<String> {
        https_location(host, path, port)
    }

    #[test]
    fn https_location_keeps_host_path_and_query() {
        assert_eq!(
            location(443, Some("example.com"), Some("/a/b?x=1&y=2")).as_deref(),
            Some("https://example.com/a/b?x=1&y=2")
        );
        assert_eq!(
            location(443, Some("example.com"), None).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn https_location_replaces_the_plain_port_with_the_tls_port() {
        // Keeping the plain port would send the client back to the plain listener (a loop)
        assert_eq!(
            location(443, Some("example.com:80"), Some("/")).as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            location(8443, Some("example.com:3500"), Some("/x")).as_deref(),
            Some("https://example.com:8443/x")
        );
        assert_eq!(
            location(8443, Some("[::1]:3500"), Some("/x")).as_deref(),
            Some("https://[::1]:8443/x")
        );
    }

    #[test]
    fn https_location_rejects_missing_or_malformed_hosts() {
        assert_eq!(location(443, None, Some("/")), None);
        assert_eq!(location(443, Some(""), Some("/")), None);
        assert_eq!(location(443, Some("evil.com/path"), Some("/")), None);
        assert_eq!(location(443, Some("a b"), Some("/")), None);
        assert_eq!(
            location(443, Some("user@example.com"), Some("/")).as_deref(),
            Some("https://example.com/")
        );
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
        assert!(route_value.runtime.state.load().config.strip_path_prefix);
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
        assert!(route_value.runtime.state.load().config.strip_path_prefix);
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
        assert!(route_value.runtime.state.load().config.strip_path_prefix);
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
    fn wildcard_virtual_hosts_match_like_nginx_server_name() {
        let router = |path: &str| {
            let mut router = Router::new();
            router.insert(path, create_test_route_value(false)).unwrap();
            router
        };
        let mut vhosts = std::collections::HashMap::new();
        vhosts.insert("www.example.com".to_string(), router("/exact"));
        vhosts.insert("*.example.com".to_string(), router("/wild"));
        vhosts.insert("*.eu.example.com".to_string(), router("/eu"));
        vhosts.insert("::1".to_string(), router("/v6"));
        let proxy = Proxy::with_vhosts(router("/default"), vhosts, 64);
        let served = |host: &str, path: &str| proxy.resolve(Some(host), path).is_ok();

        assert!(served("www.example.com", "/exact"), "exact before wildcard");
        assert!(!served("www.example.com", "/wild"));
        assert!(served("api.example.com", "/wild"));
        assert!(
            served("A.B.Example.COM.:443", "/wild"),
            "any depth, case, dot, port"
        );
        assert!(served("shop.eu.example.com", "/eu"), "longest wildcard");
        assert!(served("example.com", "/default"), "not the bare domain");
        assert!(served("[::1]:8080", "/v6"));
    }

    #[test]
    fn test_regex_route_fallback() {
        let mut default_router = Router::new();
        default_router
            .insert("/api", create_test_route_value(false))
            .unwrap();

        let regex_routes = vec![(
            Regex::new(r"\.(jpg|png)$").unwrap(),
            create_test_route_value(false),
        )];

        let proxy = Proxy::with_regex_routes(
            default_router,
            regex_routes,
            std::collections::HashMap::new(),
            64,
        );

        // matchit route still wins.
        assert!(proxy.resolve(None, "/api").is_ok());
        // regex fallback matches and forwards the full path (no stripping).
        let r = proxy.resolve(None, "/images/cat.png").expect("regex match");
        assert!(r.stripped_path.is_none());
        // no matchit and no regex match -> not found.
        assert!(proxy.resolve(None, "/images/cat.gif").is_err());
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
