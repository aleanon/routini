//! Per-route runtime configuration shared by the proxy's request/response filters.
//!
//! [`RouteConfig`] holds everything configurable per route (header rules, etc.). [`RouteRuntime`]
//! bundles a route's config with its load balancer and is what the proxy stores per route and
//! caches per concrete request path.
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ipnet::IpNet;

use http::{HeaderName, HeaderValue, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::HttpPeer;
use pingora::protocols::l4::socket::SocketAddr;
use pingora_limits::inflight::{Guard, Inflight};
use pingora_limits::rate::Rate;

use crate::adaptive_loadbalancer::{
    AdaptiveBackend, AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine,
};

pub type SharedLb = Arc<AdaptiveLoadBalancer<AdaptiveDecisionEngine>>;

/// Per-request failover behaviour (nginx `proxy_next_upstream` for connect errors).
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Maximum additional upstream attempts after the first, each on a different backend.
    pub max_retries: usize,
    pub retry_on_connect_error: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 1,
            retry_on_connect_error: true,
        }
    }
}

/// Passive health checking (nginx `max_fails` / `fail_timeout`): eject a backend after it accrues
/// `max_fails` connection failures within `fail_timeout`, and restore it once `fail_timeout` elapses.
#[derive(Debug, Clone, Copy)]
pub struct PassiveHealthConfig {
    /// Off by default — routini already runs active health checks; this is opt-in.
    pub enabled: bool,
    pub max_fails: u32,
    pub fail_timeout: Duration,
}

impl Default for PassiveHealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_fails: 3,
            fail_timeout: Duration::from_secs(10),
        }
    }
}

/// IP allow/deny and HTTP Basic auth for a route (nginx `allow`/`deny`, `auth_basic`).
#[derive(Debug, Clone, Default)]
pub struct AccessControl {
    /// If non-empty, only IPs within these networks are permitted (allow-list).
    pub allow: Vec<IpNet>,
    /// IPs within these networks are rejected (takes precedence over `allow`).
    pub deny: Vec<IpNet>,
    pub basic_auth_realm: Option<String>,
    /// Accepted credentials as base64(`user:password`) tokens. Empty = no auth required.
    pub basic_auth: HashSet<String>,
}

impl AccessControl {
    /// Whether `ip` passes the deny-then-allow IP rules.
    pub fn ip_allowed(&self, ip: IpAddr) -> bool {
        if self.deny.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        true
    }

    pub fn requires_auth(&self) -> bool {
        !self.basic_auth.is_empty()
    }

    /// Validate an `Authorization` header value against the configured Basic credentials.
    pub fn auth_ok(&self, authorization: Option<&str>) -> bool {
        if self.basic_auth.is_empty() {
            return true;
        }
        match authorization.and_then(|h| h.strip_prefix("Basic ")) {
            Some(token) => self.basic_auth.contains(token.trim()),
            None => false,
        }
    }
}

/// Response caching for a route (nginx `proxy_cache`). Responses are cached in a shared in-memory
/// store; origin `Cache-Control` is honored, otherwise 200 responses are cached for `ttl`.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl: Duration,
}

/// An immediate response a route can return without proxying (nginx `return` / `rewrite ... redirect`).
#[derive(Debug, Clone)]
pub enum RouteAction {
    /// Redirect to `location`. The template may contain `$uri` (request path) and `$request_uri`
    /// (path + query), substituted per request.
    Redirect { status: u16, location: String },
    /// Respond immediately with `status` and an optional body.
    Return { status: u16, body: Option<String> },
}

/// Per-client-IP request-rate limiter (nginx `limit_req`). Backed by a 1-second sliding window.
pub struct RateLimiter {
    rate: Rate,
    max_rps: f64,
}

impl RateLimiter {
    pub fn new(max_rps: f64) -> Self {
        Self {
            rate: Rate::new(Duration::from_secs(1)),
            max_rps,
        }
    }

    /// Record a request from `key` and return `true` if it is over the limit (reject).
    pub fn over_limit(&self, key: &IpAddr) -> bool {
        self.rate.observe(key, 1);
        self.rate.rate(key) > self.max_rps
    }
}

/// Per-client-IP concurrent-request limiter (nginx `limit_conn`).
pub struct ConnLimiter {
    inflight: Inflight,
    max: isize,
}

impl ConnLimiter {
    pub fn new(max: usize) -> Self {
        Self {
            inflight: Inflight::new(),
            max: max as isize,
        }
    }

    /// Try to reserve a concurrency slot for `key`. On success returns a [`Guard`] that must be
    /// held for the request's lifetime (it decrements the count on drop). On `Err` the request is
    /// over the limit (the transient increment is already rolled back by dropping the guard).
    pub fn acquire(&self, key: &IpAddr) -> Result<Guard, ()> {
        let (guard, count) = self.inflight.incr(*key, 1);
        if count > self.max { Err(()) } else { Ok(guard) }
    }
}

/// Settings for connecting to the upstream (nginx `proxy_pass https://`, gRPC/HTTP2 upstreams).
#[derive(Debug, Clone)]
pub struct UpstreamTls {
    pub enabled: bool,
    /// SNI / certificate hostname to present and verify against.
    pub sni: Option<String>,
    /// Verify the upstream certificate and hostname.
    pub verify: bool,
    /// Negotiate HTTP/2 to the upstream (required for gRPC). Over TLS this is via ALPN.
    pub h2: bool,
}

impl Default for UpstreamTls {
    fn default() -> Self {
        Self {
            enabled: false,
            sni: None,
            verify: true,
            h2: false,
        }
    }
}

struct FailWindow {
    count: u32,
    window_start: Instant,
}

struct Ejection {
    backend: AdaptiveBackend,
    until: Instant,
}

/// Runtime state for [`PassiveHealthConfig`]: tracks per-backend failure windows and ejections.
/// Lives in [`RouteRuntime`] (not [`RouteConfig`]) because it carries mutable, shared state.
pub struct PassiveHealth {
    config: PassiveHealthConfig,
    windows: Mutex<HashMap<SocketAddr, FailWindow>>,
    ejections: Mutex<Vec<Ejection>>,
}

impl PassiveHealth {
    pub fn new(config: PassiveHealthConfig) -> Self {
        Self {
            config,
            windows: Mutex::new(HashMap::new()),
            ejections: Mutex::new(Vec::new()),
        }
    }

    /// Record a connection failure. Returns `true` when the backend has crossed `max_fails`
    /// within the window and should be ejected by the caller.
    pub fn record_failure(&self, backend: &AdaptiveBackend) -> bool {
        if !self.config.enabled {
            return false;
        }
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap();
        let window = windows
            .entry(backend.addr.clone())
            .or_insert(FailWindow { count: 0, window_start: now });
        if now.duration_since(window.window_start) > self.config.fail_timeout {
            window.count = 0;
            window.window_start = now;
        }
        window.count += 1;
        if window.count >= self.config.max_fails {
            windows.remove(&backend.addr);
            self.ejections.lock().unwrap().push(Ejection {
                backend: backend.clone(),
                until: now + self.config.fail_timeout,
            });
            return true;
        }
        false
    }

    /// Clear the failure window for a backend after a successful response.
    pub fn record_success(&self, backend: &AdaptiveBackend) {
        if !self.config.enabled {
            return;
        }
        self.windows.lock().unwrap().remove(&backend.addr);
    }

    /// Return backends whose ejection window has elapsed, so the caller can re-enable them.
    pub fn take_expired(&self, now: Instant) -> Vec<AdaptiveBackend> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut ejections = self.ejections.lock().unwrap();
        let mut expired = Vec::new();
        ejections.retain(|e| {
            if e.until <= now {
                expired.push(e.backend.clone());
                false
            } else {
                true
            }
        });
        expired
    }
}

/// Upstream connection/transfer timeouts, applied to the [`HttpPeer`] per request.
/// `None` leaves Pingora's default in place. Equivalent to nginx `proxy_connect_timeout`,
/// `proxy_read_timeout`, `proxy_send_timeout`.
#[derive(Debug, Clone, Default)]
pub struct TimeoutConfig {
    pub connect: Option<Duration>,
    pub read: Option<Duration>,
    pub write: Option<Duration>,
    pub idle: Option<Duration>,
}

impl TimeoutConfig {
    /// Overlay any configured timeouts onto a peer's options.
    pub fn apply(&self, peer: &mut HttpPeer) {
        if let Some(connect) = self.connect {
            peer.options.connection_timeout = Some(connect);
        }
        if let Some(read) = self.read {
            peer.options.read_timeout = Some(read);
        }
        if let Some(write) = self.write {
            peer.options.write_timeout = Some(write);
        }
        if let Some(idle) = self.idle {
            peer.options.idle_timeout = Some(idle);
        }
    }
}

/// Policy for the `Host` header sent to the upstream.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum HostRewrite {
    /// Forward the client's original `Host` header unchanged (nginx `proxy_set_header Host $host`).
    #[default]
    Preserve,
    /// Replace the `Host` header with a fixed value.
    Set(String),
}

/// Request/response header manipulation rules, the equivalent of nginx's `proxy_set_header`,
/// `add_header`, and the forwarded-header conventions.
#[derive(Debug, Clone)]
pub struct HeaderRules {
    /// Add `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Real-IP`.
    pub forwarded: bool,
    /// When `true`, append the client IP to an existing `X-Forwarded-For` (we are behind a trusted
    /// proxy). When `false`, reset `X-Forwarded-For` to just the direct client IP to avoid spoofing.
    pub trusted_proxy: bool,
    pub host: HostRewrite,
    pub set_request: Vec<(HeaderName, HeaderValue)>,
    pub remove_request: Vec<HeaderName>,
    pub add_response: Vec<(HeaderName, HeaderValue)>,
    pub remove_response: Vec<HeaderName>,
}

impl Default for HeaderRules {
    fn default() -> Self {
        Self {
            // A reverse proxy should advertise the real client by default.
            forwarded: true,
            trusted_proxy: false,
            host: HostRewrite::Preserve,
            set_request: Vec::new(),
            remove_request: Vec::new(),
            add_response: Vec::new(),
            remove_response: Vec::new(),
        }
    }
}

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");

impl HeaderRules {
    /// Apply request-header rules to the outgoing upstream request.
    ///
    /// `client_ip`/`client_tls` describe the downstream connection and drive the forwarded headers.
    pub fn apply_request(
        &self,
        upstream: &mut RequestHeader,
        client_ip: Option<IpAddr>,
        client_tls: bool,
    ) {
        if let HostRewrite::Set(value) = &self.host {
            if let Ok(v) = HeaderValue::from_str(value) {
                let _ = upstream.insert_header(header::HOST, v);
            }
        }

        if self.forwarded {
            if let Some(ip) = client_ip {
                let ip_str = ip.to_string();
                let _ = upstream.insert_header(X_REAL_IP, &ip_str);

                let xff = if self.trusted_proxy {
                    match upstream.headers.get(&X_FORWARDED_FOR) {
                        Some(existing) => format!(
                            "{}, {ip_str}",
                            existing.to_str().unwrap_or_default()
                        ),
                        None => ip_str.clone(),
                    }
                } else {
                    ip_str
                };
                let _ = upstream.insert_header(X_FORWARDED_FOR, &xff);

                let proto = if client_tls { "https" } else { "http" };
                let _ = upstream.insert_header(X_FORWARDED_PROTO, proto);
            }
        }

        for (name, value) in &self.set_request {
            let _ = upstream.insert_header(name.clone(), value.clone());
        }
        for name in &self.remove_request {
            upstream.remove_header(name);
        }
    }

    /// Apply response-header rules to the downstream response.
    pub fn apply_response(&self, response: &mut ResponseHeader) {
        for (name, value) in &self.add_response {
            let _ = response.insert_header(name.clone(), value.clone());
        }
        for name in &self.remove_response {
            response.remove_header(name);
        }
    }
}

/// All per-route configuration. Built once (from the builder or config file) and shared read-only.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// Strip the matched route prefix before forwarding (see the route docs).
    pub strip_path_prefix: bool,
    pub headers: HeaderRules,
    pub timeouts: TimeoutConfig,
    pub retry: RetryConfig,
    pub passive_health: PassiveHealthConfig,
    /// Maximum request body size in bytes (nginx `client_max_body_size`). `None` = unlimited.
    pub max_body_size: Option<usize>,
    pub upstream_tls: UpstreamTls,
    /// `Strict-Transport-Security` header value to add on TLS responses (nginx HSTS `add_header`).
    pub hsts: Option<String>,
    /// Max requests/sec per client IP (nginx `limit_req`). `None` = unlimited.
    pub rate_limit_rps: Option<f64>,
    /// Max concurrent requests per client IP (nginx `limit_conn`). `None` = unlimited.
    pub max_connections: Option<usize>,
    /// If set, the route short-circuits with this response instead of proxying (redirect/return).
    pub action: Option<RouteAction>,
    /// Response caching (nginx `proxy_cache`). `None` = no caching.
    pub cache: Option<CacheConfig>,
    /// IP allow/deny + Basic auth (nginx `allow`/`deny`/`auth_basic`). `None` = open.
    pub access: Option<AccessControl>,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            strip_path_prefix: true,
            headers: HeaderRules::default(),
            timeouts: TimeoutConfig::default(),
            retry: RetryConfig::default(),
            passive_health: PassiveHealthConfig::default(),
            max_body_size: None,
            upstream_tls: UpstreamTls::default(),
            hsts: None,
            rate_limit_rps: None,
            max_connections: None,
            action: None,
            cache: None,
            access: None,
        }
    }
}

/// The hot-swappable part of a route: its config plus the runtime state derived from it
/// (passive-health tracker, rate/connection limiters). Replaced atomically on config reload.
pub struct RouteState {
    pub config: RouteConfig,
    pub health: PassiveHealth,
    pub rate_limiter: Option<RateLimiter>,
    pub conn_limiter: Option<ConnLimiter>,
}

impl RouteState {
    pub fn new(config: RouteConfig) -> Self {
        let health = PassiveHealth::new(config.passive_health);
        let rate_limiter = config.rate_limit_rps.map(RateLimiter::new);
        let conn_limiter = config.max_connections.map(ConnLimiter::new);
        Self {
            config,
            health,
            rate_limiter,
            conn_limiter,
        }
    }
}

/// A route's load balancer plus its hot-swappable [`RouteState`]. The `lb` (backends + strategy)
/// is fixed for the process lifetime; `state` is read lock-free per request and can be replaced on
/// SIGHUP config reload via [`RouteRuntime::reload`].
pub struct RouteRuntime {
    pub lb: SharedLb,
    pub state: arc_swap::ArcSwap<RouteState>,
}

impl RouteRuntime {
    pub fn new(lb: SharedLb, config: RouteConfig) -> Self {
        Self {
            lb,
            state: arc_swap::ArcSwap::from_pointee(RouteState::new(config)),
        }
    }

    /// Atomically replace this route's config-derived state (used by config reload). Limiter and
    /// passive-health counters reset, matching nginx's "new workers, fresh limit state" reload.
    pub fn reload(&self, config: RouteConfig) {
        self.state.store(Arc::new(RouteState::new(config)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> RequestHeader {
        RequestHeader::build("GET", b"/", None).unwrap()
    }

    #[test]
    fn forwarded_headers_set_for_direct_client() {
        let rules = HeaderRules::default();
        let mut r = req();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), true);

        assert_eq!(r.headers.get(&X_REAL_IP).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_PROTO).unwrap(), "https");
    }

    #[test]
    fn untrusted_proxy_resets_xff() {
        let rules = HeaderRules::default(); // trusted_proxy = false
        let mut r = req();
        r.insert_header(X_FORWARDED_FOR, "1.2.3.4").unwrap();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), false);

        // spoofed upstream value is discarded, only the real client IP remains
        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_PROTO).unwrap(), "http");
    }

    #[test]
    fn trusted_proxy_appends_xff() {
        let rules = HeaderRules {
            trusted_proxy: true,
            ..Default::default()
        };
        let mut r = req();
        r.insert_header(X_FORWARDED_FOR, "1.2.3.4").unwrap();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), false);

        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "1.2.3.4, 203.0.113.5");
    }

    #[test]
    fn host_rewrite_and_custom_headers() {
        let rules = HeaderRules {
            forwarded: false,
            host: HostRewrite::Set("upstream.internal".into()),
            set_request: vec![(
                HeaderName::from_static("x-api"),
                HeaderValue::from_static("1"),
            )],
            remove_request: vec![HeaderName::from_static("cookie")],
            ..Default::default()
        };
        let mut r = req();
        r.insert_header(http::header::COOKIE, "secret").unwrap();
        rules.apply_request(&mut r, None, false);

        assert_eq!(r.headers.get(http::header::HOST).unwrap(), "upstream.internal");
        assert_eq!(r.headers.get("x-api").unwrap(), "1");
        assert!(r.headers.get(http::header::COOKIE).is_none());
    }

    #[test]
    fn timeouts_overlay_peer_options() {
        let cfg = TimeoutConfig {
            connect: Some(Duration::from_millis(500)),
            read: Some(Duration::from_secs(5)),
            write: None,
            idle: None,
        };
        let mut peer = HttpPeer::new("127.0.0.1:8080", false, String::new());
        let default_write = peer.options.write_timeout;
        cfg.apply(&mut peer);

        assert_eq!(peer.options.connection_timeout, Some(Duration::from_millis(500)));
        assert_eq!(peer.options.read_timeout, Some(Duration::from_secs(5)));
        // unset fields are left untouched
        assert_eq!(peer.options.write_timeout, default_write);
    }

    #[test]
    fn passive_health_ejects_after_max_fails_then_restores() {
        let cfg = PassiveHealthConfig {
            enabled: true,
            max_fails: 2,
            fail_timeout: Duration::from_millis(50),
        };
        let ph = PassiveHealth::new(cfg);
        let backend = AdaptiveBackend::build("127.0.0.1:9001", 1).unwrap();

        assert!(!ph.record_failure(&backend), "first failure should not eject");
        assert!(ph.record_failure(&backend), "second failure should eject");

        // still ejected immediately after
        assert!(ph.take_expired(Instant::now()).is_empty());

        // after fail_timeout the backend becomes eligible for restore
        let later = Instant::now() + Duration::from_millis(60);
        let expired = ph.take_expired(later);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].addr, backend.addr);
    }

    #[test]
    fn access_control_ip_and_basic_auth() {
        let ac = AccessControl {
            allow: vec!["10.0.0.0/8".parse().unwrap()],
            deny: vec!["10.1.2.3".parse::<IpAddr>().unwrap().into()],
            basic_auth: ["dXNlcjpwYXNz".to_string()].into_iter().collect(), // base64("user:pass")
            ..Default::default()
        };

        assert!(ac.ip_allowed("10.5.5.5".parse().unwrap()), "in allow net");
        assert!(!ac.ip_allowed("10.1.2.3".parse().unwrap()), "explicitly denied");
        assert!(!ac.ip_allowed("192.168.0.1".parse().unwrap()), "outside allow net");

        assert!(ac.requires_auth());
        assert!(ac.auth_ok(Some("Basic dXNlcjpwYXNz")));
        assert!(!ac.auth_ok(Some("Basic d3Jvbmc=")));
        assert!(!ac.auth_ok(None));
    }

    #[test]
    fn conn_limiter_caps_concurrency_and_frees_on_drop() {
        let cl = ConnLimiter::new(2);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        let g1 = cl.acquire(&ip).expect("1st under limit");
        let _g2 = cl.acquire(&ip).expect("2nd under limit");
        assert!(cl.acquire(&ip).is_err(), "3rd is over the limit of 2");

        drop(g1); // free a slot
        assert!(cl.acquire(&ip).is_ok(), "slot reusable after a guard drops");
    }

    #[test]
    fn passive_health_disabled_never_ejects() {
        let ph = PassiveHealth::new(PassiveHealthConfig::default()); // enabled = false
        let backend = AdaptiveBackend::build("127.0.0.1:9001", 1).unwrap();
        for _ in 0..5 {
            assert!(!ph.record_failure(&backend));
        }
        assert!(ph.take_expired(Instant::now()).is_empty());
    }

    #[test]
    fn response_headers_added_and_removed() {
        let rules = HeaderRules {
            add_response: vec![(
                HeaderName::from_static("x-served-by"),
                HeaderValue::from_static("routini"),
            )],
            remove_response: vec![HeaderName::from_static("server")],
            ..Default::default()
        };
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header(http::header::SERVER, "secret-server").unwrap();
        rules.apply_response(&mut resp);

        assert_eq!(resp.headers.get("x-served-by").unwrap(), "routini");
        assert!(resp.headers.get(http::header::SERVER).is_none());
    }
}
