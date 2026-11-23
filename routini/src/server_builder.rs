use std::{
    collections::HashMap,
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, LazyLock},
    thread,
    time::Duration,
};

use color_eyre::eyre::{Result, eyre};
use pingora::{
    listeners::{
        AcceptLimits, AcceptStats, DEFAULT_HANDSHAKE_TIMEOUT, ListenerConfig, tls::TlsSettings,
    },
    prelude::background_service,
    proxy::http_proxy_service,
    server::{
        Server,
        configuration::{Opt, ServerConf},
    },
};
use regex::Regex;

use crate::{
    adaptive_loadbalancer::{AdaptiveBackends, options::AdaptiveLbOpt},
    config::Config,
    load_balancing::{
        Backends,
        discovery::{Upstream, UpstreamDiscovery},
        strategy::{Adaptive, adaptive::AdaptiveStrategyMetrics},
    },
    proxy::ClientTimeouts,
    real_ip::RealIp,
    reload::RouteManager,
    tls::{self, TlsOptions},
    utils::constants::{
        DEFAULT_GRACE_PERIOD, DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT, DEFAULT_WILDCARD_IDENTIFIER,
        PROMETHEUS_ENDPOINT_ADDRESS,
    },
};

// Re-export so existing `server_builder::RouteConfig` references keep working; the canonical
// definition now lives in `crate::route` alongside the rest of the per-route runtime config.
pub use crate::route::RouteConfig;

/// Limits on the connections each proxy listener (plain and TLS, counted separately) accepts.
/// Connections over a limit are closed right after `accept`, before any work is done for them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListenerLimits {
    /// Open connections per listener (nginx `worker_connections`). Default: no limit.
    pub max_connections: Option<usize>,
    /// Connections per listener whose TLS handshake hasn't finished. Default: no limit.
    pub max_pending_handshakes: Option<usize>,
    /// Longest a TLS handshake may take, including any wait for a handshake thread. Default 60s.
    pub handshake_timeout: Duration,
    /// Close a connection its client already closed or reset instead of starting its handshake
    /// (Linux). Default: off.
    pub skip_closed_peers: bool,
}

impl Default for ListenerLimits {
    fn default() -> Self {
        Self {
            max_connections: None,
            max_pending_handshakes: None,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            skip_closed_peers: false,
        }
    }
}

impl ListenerLimits {
    /// The limits for one listener, counting into a fresh [AcceptStats] exported as `listener`.
    fn for_listener(&self, listener: &'static str) -> AcceptLimits {
        let stats = Arc::new(AcceptStats::default());
        crate::metrics::watch_listener(listener, &stats);
        AcceptLimits {
            handshake_timeout: self.handshake_timeout,
            max_connections: self.max_connections,
            max_pending_handshakes: self.max_pending_handshakes,
            skip_closed_peers: self.skip_closed_peers,
            stats: Some(stats),
        }
    }
}

/// TLS termination settings for the proxy's public listener.
pub struct TlsConfig {
    pub address: String,
    /// Certificates, protocol versions, client authentication and ALPN, see [crate::tls].
    pub options: TlsOptions,
    /// Run handshakes on this many pools of this many threads each, instead of on the threads
    /// serving requests, so a burst of new clients doesn't hold up requests in flight.
    pub handshake_offload: Option<HandshakeOffload>,
}

/// Dedicated handshake threads: connections are spread over `pools` by connection id, and over
/// the `threads_per_pool` single-threaded runtimes of a pool at random.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandshakeOffload {
    pub pools: usize,
    pub threads_per_pool: usize,
}

const PATH_REGEX_PATTERN: &str = r"^/[^*]*\*?$";
static PATH_REGEX: LazyLock<Regex> =
    LazyLock::new(|| regex::Regex::new(PATH_REGEX_PATTERN).unwrap());

pub type Application = Server;

pub fn proxy_server(listener: TcpListener) -> ServerBuilder {
    let address = listener
        .local_addr()
        .expect("Invalid socket address")
        .to_string();
    ServerBuilder::new(address)
}

impl ServerBuilder {
    /// A builder listening on `address`. Unlike [proxy_server] this does not bind first, which
    /// a graceful upgrade needs as the old process still holds the port.
    pub fn new(address: impl Into<String>) -> Self {
        ServerBuilder {
            address: address.into(),
            routes: Vec::new(),
            server_config: None,
            tls: None,
            prometheus_address: None,
            access_log: true,
            https_redirect: false,
            compression_level: 0,
            reload_config_path: None,
            applied_config: None,
            request_id: false,
            error_pages: HashMap::new(),
            real_ip: None,
            client_timeouts: ClientTimeouts::default(),
            listener_limits: ListenerLimits::default(),
            upgrade: false,
            pid_file: None,
            upgrade_sock: None,
        }
    }
}

pub struct Route {
    pub path: String,
    pub backends: AdaptiveBackends,
    /// Full tuning for the route's adaptive load balancer. `starting_strategy`,
    /// `max_iterations` and `health_check_interval` are also adjustable via the builder methods.
    pub lb_options: AdaptiveLbOpt,
    pub route_config: RouteConfig,
    /// Optional virtual host this route belongs to (nginx `server_name`). `None` = default server.
    pub host: Option<String>,
    /// When true, `path` is a regex location matched against the full request path (default server
    /// only), tried after matchit routes (nginx `location ~ <regex>`).
    pub is_regex: bool,
    /// Handle to the discovery inside [Self::backends], used to change the upstreams of a
    /// running route on config reload.
    pub(crate) discovery: Arc<UpstreamDiscovery<AdaptiveStrategyMetrics>>,
}

impl Route {
    pub fn new<A: AsRef<str>>(
        path: impl AsRef<str>,
        backends: impl IntoIterator<Item = A>,
        lb_strategy: Adaptive,
    ) -> Result<Self> {
        let lb_options = AdaptiveLbOpt {
            starting_strategy: lb_strategy,
            ..Default::default()
        };
        Self::with_options(path, backends, lb_options)
    }

    /// Construct a route with a fully specified [AdaptiveLbOpt], e.g. when building from config.
    /// Every backend is given weight 1; use [`Route::with_weighted_backends`] for weighted pools.
    pub fn with_options<A: AsRef<str>>(
        path: impl AsRef<str>,
        backends: impl IntoIterator<Item = A>,
        lb_options: AdaptiveLbOpt,
    ) -> Result<Self> {
        let weighted = backends
            .into_iter()
            .map(|addr| (addr.as_ref().to_string(), 1usize));
        Self::with_weighted_backends(path, weighted, lb_options)
    }

    /// Construct a route from weighted `(address, weight)` upstreams. Weight proportionally biases
    /// the load-balancing selectors (nginx `server <addr> weight=N`).
    pub fn with_weighted_backends(
        path: impl AsRef<str>,
        backends: impl IntoIterator<Item = (String, usize)>,
        lb_options: AdaptiveLbOpt,
    ) -> Result<Self> {
        if !PATH_REGEX.is_match(path.as_ref()) {
            return Err(eyre!(
                "Invalid path, it must start with '/', have at most one * and any eventual * must be at the end of the string"
            ));
        }
        let path = path.as_ref().replacen("*", DEFAULT_WILDCARD_IDENTIFIER, 1);
        let discovery = Self::build_discovery(backends)?;

        Ok(Route {
            path,
            backends: Backends::new(Box::new(discovery.clone())),
            discovery,
            lb_options,
            route_config: RouteConfig::default(),
            host: None,
            is_regex: false,
        })
    }

    /// Construct a regex location route from weighted upstreams (nginx `location ~ <pattern>`).
    /// The `pattern` is matched against the full request path; the path is forwarded unchanged.
    pub fn regex(
        pattern: impl AsRef<str>,
        backends: impl IntoIterator<Item = (String, usize)>,
        lb_options: AdaptiveLbOpt,
    ) -> Result<Self> {
        Regex::new(pattern.as_ref()).map_err(|e| eyre!("Invalid regex pattern: {e}"))?;
        let discovery = Self::build_discovery(backends)?;

        Ok(Route {
            path: pattern.as_ref().to_string(),
            backends: Backends::new(Box::new(discovery.clone())),
            discovery,
            lb_options,
            route_config: RouteConfig::default(),
            host: None,
            is_regex: true,
        })
    }

    /// Upstreams are `ip:port` or `hostname:port`; names are resolved by the route's
    /// background task (see [UpstreamDiscovery]), not here.
    fn build_discovery(
        backends: impl IntoIterator<Item = (String, usize)>,
    ) -> Result<Arc<UpstreamDiscovery<AdaptiveStrategyMetrics>>> {
        let upstreams = backends
            .into_iter()
            .map(|(addr, weight)| {
                Upstream::parse(&addr, weight.max(1))
                    .map_err(|err| eyre!("Invalid backend address '{addr}': {err}"))
            })
            .collect::<Result<Vec<_>>>()?;
        if upstreams.is_empty() {
            return Err(eyre!("Must provide at least one backend"));
        }
        Ok(Arc::new(UpstreamDiscovery::new(upstreams)))
    }

    /// Restrict this route to a named virtual host (nginx `server_name`).
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Default: [DEFAULT_MAX_ALGORITHM_ITERATIONS](crate::utils::constants::DEFAULT_MAX_ALGORITHM_ITERATIONS)
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.lb_options.max_iterations = max_iterations;
        self
    }

    /// Default: Some(Duration::from_secs(1))
    /// set to None to not run health check background service
    pub fn include_health_check(mut self, update_frequency: Option<Duration>) -> Self {
        self.lb_options.health_check_interval = update_frequency;
        self
    }

    /// #### Default:
    /// ```rust
    /// RouteConfig {
    ///     strip_prefix: true,
    /// }
    /// ```
    pub fn route_config(mut self, config: RouteConfig) -> Self {
        self.route_config = config;
        self
    }
}

pub struct ServerBuilder {
    address: String,
    routes: Vec<Route>,
    server_config: Option<ServerConf>,
    tls: Option<TlsConfig>,
    prometheus_address: Option<String>,
    access_log: bool,
    https_redirect: bool,
    compression_level: u32,
    reload_config_path: Option<String>,
    applied_config: Option<Config>,
    request_id: bool,
    error_pages: HashMap<u16, String>,
    real_ip: Option<RealIp>,
    client_timeouts: ClientTimeouts,
    listener_limits: ListenerLimits,
    upgrade: bool,
    pid_file: Option<String>,
    upgrade_sock: Option<String>,
}
impl ServerBuilder {
    pub fn add_route(mut self, route: impl Into<Route>) -> Self {
        self.routes.push(route.into());
        self
    }

    pub fn server_config(mut self, server_config: ServerConf) -> Self {
        self.server_config = Some(server_config);
        self
    }

    /// Terminate TLS on `address`, serving the same routes as the plain listener.
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Override the Prometheus metrics listen address.
    /// Default: [PROMETHEUS_ENDPOINT_ADDRESS].
    pub fn prometheus_address(mut self, addr: String) -> Self {
        self.prometheus_address = Some(addr);
        self
    }

    /// Enable/disable the per-request access log (nginx `access_log on/off`). Default: on.
    pub fn access_log(mut self, enabled: bool) -> Self {
        self.access_log = enabled;
        self
    }

    /// Redirect plain-HTTP requests to their `https://` equivalent. Default: off.
    pub fn https_redirect(mut self, enabled: bool) -> Self {
        self.https_redirect = enabled;
        self
    }

    /// Downstream response compression level (nginx `gzip`); 0 disables. Default: 0.
    pub fn compression_level(mut self, level: u32) -> Self {
        self.compression_level = level;
        self
    }

    /// Re-read and apply the config file at `path` on `SIGHUP` (nginx `-s reload`), see
    /// [crate::reload] for what changes apply live. Without it SIGHUP only logs an error.
    pub fn reload_on_sighup(mut self, path: String) -> Self {
        self.reload_config_path = Some(path);
        self
    }

    /// Take over the listening sockets of a running routini instead of binding them (graceful
    /// upgrade). Start the new process with this, then send SIGQUIT to the old one.
    pub fn upgrade(mut self, upgrade: bool) -> Self {
        self.upgrade = upgrade;
        self
    }

    /// Unix socket the listening sockets are handed over on during a graceful upgrade. Old and
    /// new process must agree. Default: pingora's `/tmp/pingora_upgrade.sock`; the config file
    /// path defaults to a private runtime directory instead.
    pub fn upgrade_sock(mut self, path: impl Into<String>) -> Self {
        self.upgrade_sock = Some(path.into());
        self
    }

    /// PID file written when daemonized. Default: pingora's `/tmp/pingora.pid`.
    pub fn pid_file(mut self, path: impl Into<String>) -> Self {
        self.pid_file = Some(path.into());
        self
    }

    /// The config the routes were built from, so reloads can tell which route settings changed
    /// and which changes need a restart. Without it every reloaded route counts as changed.
    pub fn applied_config(mut self, config: Config) -> Self {
        self.applied_config = Some(config);
        self
    }

    /// Generate/propagate an `X-Request-Id` header for request tracing. Default: off.
    pub fn request_id(mut self, enabled: bool) -> Self {
        self.request_id = enabled;
        self
    }

    /// Custom error-page bodies keyed by status code (nginx `error_page`).
    pub fn error_pages(mut self, pages: HashMap<u16, String>) -> Self {
        self.error_pages = pages;
        self
    }

    /// Take the client address from a header when the peer is a trusted proxy (nginx
    /// `real_ip`). Rate/connection limits, allow/deny, hashing and the access log then see the
    /// client instead of the proxy.
    pub fn real_ip(mut self, real_ip: RealIp) -> Self {
        self.real_ip = Some(real_ip);
        self
    }

    /// Timeouts on client connections. Default: nginx's (keepalive 75s, body and send 60s).
    pub fn client_timeouts(mut self, timeouts: ClientTimeouts) -> Self {
        self.client_timeouts = timeouts;
        self
    }

    /// Limits on the connections the proxy listeners accept. Default: none.
    pub fn listener_limits(mut self, limits: ListenerLimits) -> Self {
        self.listener_limits = limits;
        self
    }

    pub fn build(self) -> Server {
        self.build_with_reload_handle().0
    }

    /// Like [Self::build], also returning the [RouteManager] that owns the routes, for applying
    /// config reloads programmatically ([RouteManager::reload]).
    pub fn build_with_reload_handle(self) -> (Server, Arc<RouteManager>) {
        assert!(!self.routes.is_empty(), "requires at least one route");
        // Use a caller-provided ServerConf as-is (it carries the configured tuning); otherwise fall
        // back to defaults with an aggressive upstream keepalive pool.
        let server_config = self.server_config.unwrap_or_else(|| {
            let mut config = ServerConf::default();
            config.upstream_keepalive_pool_size = 200000;
            config
        });
        let mut server_config = server_config;
        // Pingora defaults to a 300s grace period, longer than orchestrators wait before SIGKILL
        server_config
            .grace_period_seconds
            .get_or_insert(DEFAULT_GRACE_PERIOD.as_secs());
        server_config
            .graceful_shutdown_timeout_seconds
            .get_or_insert(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT.as_secs());
        if let Some(pid_file) = self.pid_file {
            server_config.pid_file = pid_file;
        }
        if let Some(upgrade_sock) = self.upgrade_sock {
            server_config.upgrade_sock = upgrade_sock;
        }
        // Daemonizing is done by the binary before any threads exist, see main.rs
        server_config.daemon = false;
        let opt = Opt {
            upgrade: self.upgrade,
            ..Default::default()
        };
        let mut server = Server::new_with_opt_and_conf(opt, server_config);

        for route in &self.routes {
            match &route.host {
                Some(host) => tracing::info!("Adding route: {} (host: {host})", route.path),
                None if route.is_regex => tracing::info!("Adding regex route: {}", route.path),
                None => tracing::info!("Adding route: {}", route.path),
            }
        }
        // Shared with the route manager, which swaps in new certificates on reload
        let tls = self.tls.map(|tls| {
            let store = tls::CertStore::load(&tls.options)
                .expect("Failed to load TLS certificates, validate the config first");
            (tls, Arc::new(tls::CertResolver::new(store)))
        });
        let (manager, mut router) = RouteManager::new(
            self.routes,
            self.reload_config_path.map(PathBuf::from),
            self.applied_config,
            tls.as_ref().map(|(_, resolver)| resolver.clone()),
        )
        .expect("Invalid route");
        router.set_access_log(self.access_log);
        router.set_https_redirect(self.https_redirect);
        if let Some(port) = tls
            .as_ref()
            .and_then(|(tls, _)| tls.address.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok())
        {
            router.set_https_port(port);
        }
        router.set_compression_level(self.compression_level);
        router.set_request_id(self.request_id);
        router.set_error_pages(self.error_pages);
        router.set_real_ip(self.real_ip);
        router.set_client_timeouts(self.client_timeouts);

        let mut manager_service = background_service("route-manager", manager);
        manager_service.threads = Some(2);
        let manager = manager_service.task();
        server.add_service(manager_service);

        let mut router_service = http_proxy_service(&server.configuration, router);
        let available_parallelism = thread::available_parallelism().map(Into::into).unwrap_or(1);
        router_service.threads = Some(available_parallelism);
        router_service.add_listener(
            ListenerConfig::tcp(self.address.clone())
                .accept_limits(self.listener_limits.for_listener("http")),
        );

        if let Some((tls, resolver)) = tls {
            let config = tls::server_config(&tls.options, resolver)
                .expect("Invalid TLS settings, validate the config first");
            let mut settings = TlsSettings::with_server_config(Arc::new(config));
            if let Some(offload) = tls.handshake_offload {
                settings.set_offload_threadpool(offload.pools, offload.threads_per_pool);
            }
            router_service.add_listener(
                ListenerConfig::tcp(tls.address.clone())
                    .tls(settings)
                    .accept_limits(self.listener_limits.for_listener("https")),
            );
            tracing::info!("TLS termination enabled on {}", tls.address);
        }

        server.add_service(router_service);

        let mut prometheus = pingora_prometheus::prometheus_http_service();
        let prometheus_address = self
            .prometheus_address
            .as_deref()
            .unwrap_or(PROMETHEUS_ENDPOINT_ADDRESS);
        prometheus.add_tcp(prometheus_address);
        server.add_service(prometheus);

        server.bootstrap();
        (server, manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::constants::DEFAULT_MAX_ALGORITHM_ITERATIONS;

    #[tokio::test]
    async fn test_route_valid_simple_path() {
        let route = Route::new("/api", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.path, "/api");

        // Initialize backends from discovery
        let strategy = route.lb_options.starting_strategy.clone();
        route.backends.update(&strategy, |_| {}).await.unwrap();

        assert_eq!(route.backends.get_backend().len(), 1);
    }

    #[test]
    fn test_route_valid_path_with_wildcard() {
        let route = Route::new("/api/*", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.path, "/api/{*rest}");
    }

    #[test]
    fn test_route_valid_nested_path_with_wildcard() {
        let route = Route::new("/api/v1/*", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.path, "/api/v1/{*rest}");
    }

    #[test]
    fn test_route_valid_root_path() {
        let route = Route::new("/", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_ok());
    }

    #[test]
    fn test_route_invalid_path_no_leading_slash() {
        let route = Route::new("api", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_err());
        if let Err(err) = route {
            assert!(err.to_string().contains("Invalid path"));
            assert!(err.to_string().contains("must start with '/'"));
        }
    }

    #[test]
    fn test_route_invalid_path_wildcard_not_at_end() {
        let route = Route::new("/api/*/users", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_err());
        if let Err(err) = route {
            assert!(err.to_string().contains("Invalid path"));
        }
    }

    #[test]
    fn test_route_invalid_path_multiple_wildcards() {
        let route = Route::new("/api/**", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_err());
        if let Err(err) = route {
            assert!(err.to_string().contains("Invalid path"));
            assert!(err.to_string().contains("at most one *"));
        }
    }

    #[test]
    fn test_route_invalid_empty_backends() {
        let empty: Vec<&str> = vec![];
        let route = Route::new("/api", empty, Adaptive::default());
        assert!(route.is_err());
        if let Err(err) = route {
            assert!(
                err.to_string()
                    .contains("Must provide at least one backend")
            );
        }
    }

    #[tokio::test]
    async fn test_route_multiple_valid_backends() {
        let route = Route::new(
            "/api",
            vec!["127.0.0.1:8080", "127.0.0.1:8081", "127.0.0.1:8082"],
            Adaptive::default(),
        );
        assert!(route.is_ok());
        let route = route.unwrap();

        // Initialize backends from discovery
        let strategy = route.lb_options.starting_strategy.clone();
        route.backends.update(&strategy, |_| {}).await.unwrap();

        assert_eq!(route.backends.get_backend().len(), 3);
    }

    #[test]
    fn test_route_builder_pattern() {
        let route = Route::new("/api/*", vec!["127.0.0.1:8080"], Adaptive::default())
            .unwrap()
            .max_iterations(20)
            .include_health_check(None)
            .route_config(RouteConfig {
                strip_path_prefix: false,
                ..Default::default()
            });

        assert_eq!(route.lb_options.max_iterations, 20);
        assert_eq!(route.lb_options.health_check_interval, None);
        assert_eq!(route.route_config.strip_path_prefix, false);
    }

    #[test]
    fn test_route_default_values() {
        let route = Route::new("/api", vec!["127.0.0.1:8080"], Adaptive::default()).unwrap();

        assert_eq!(
            route.lb_options.max_iterations,
            DEFAULT_MAX_ALGORITHM_ITERATIONS
        );
        assert_eq!(
            route.lb_options.health_check_interval,
            Some(Duration::from_secs(1))
        );
        assert_eq!(route.route_config.strip_path_prefix, true);
    }

    #[test]
    fn test_route_config_default() {
        let config = RouteConfig::default();
        assert_eq!(config.strip_path_prefix, true);
    }

    #[test]
    fn test_route_path_with_segments() {
        let route = Route::new(
            "/api/v1/users/{id}/*",
            vec!["127.0.0.1:8080"],
            Adaptive::default(),
        );
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.path, "/api/v1/users/{id}/{*rest}");
    }

    #[test]
    fn test_path_regex_validation() {
        // Direct regex tests
        assert!(PATH_REGEX.is_match("/api"));
        assert!(PATH_REGEX.is_match("/api/*"));
        assert!(PATH_REGEX.is_match("/api/v1/users/*"));
        assert!(PATH_REGEX.is_match("/"));

        assert!(!PATH_REGEX.is_match("api"));
        assert!(!PATH_REGEX.is_match("/api/*/users"));
        assert!(!PATH_REGEX.is_match("/api/**"));
        assert!(!PATH_REGEX.is_match("/**/api"));
    }
}
