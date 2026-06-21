use std::{
    collections::{BTreeSet, HashMap},
    net::TcpListener,
    sync::{Arc, LazyLock},
    thread,
    time::Duration,
};

use color_eyre::eyre::{Result, eyre};
use matchit::Router;
use pingora::{
    listeners::tls::TlsSettings,
    prelude::{HttpPeer, background_service},
    proxy::http_proxy_service,
    server::{Server, configuration::ServerConf},
    services::listening::Service,
};
use regex::Regex;

use crate::{
    adaptive_loadbalancer::{
        AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine, options::AdaptiveLbOpt,
    },
    load_balancing::{Backend, Backends, discovery::Static, strategy::Adaptive},
    proxy::{Proxy, RouteValue},
    reload::{RouteRegistry, spawn_reload_watcher},
    route::RouteRuntime,
    set_strategy_endpoint::SetStrategyEndpoint,
    utils::constants::{
        DEFAULT_PATH_CACHE_CAPACITY, DEFAULT_WILDCARD_IDENTIFIER, PROMETHEUS_ENDPOINT_ADDRESS,
    },
};

// Re-export so existing `server_builder::RouteConfig` references keep working; the canonical
// definition now lives in `crate::route` alongside the rest of the per-route runtime config.
pub use crate::route::RouteConfig;

/// TLS termination settings for the proxy's public listener.
pub struct TlsConfig {
    pub address: String,
    pub cert_path: String,
    pub key_path: String,
    /// Advertise HTTP/2 (and HTTP/1.1) via ALPN. Off means HTTP/1.1 only.
    pub enable_h2: bool,
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

    ServerBuilder {
        address,
        routes: Vec::new(),
        set_strategy_endpoint: None,
        server_config: None,
        tls: None,
        prometheus_address: None,
        access_log: true,
        https_redirect: false,
        compression_level: 0,
        reload_config_path: None,
        request_id: false,
        error_pages: HashMap::new(),
    }
}

pub struct Route {
    pub path: String,
    pub backends: Backends,
    /// Full tuning for the route's adaptive load balancer. `starting_strategy`,
    /// `max_iterations` and `health_check_interval` are also adjustable via the builder methods.
    pub lb_options: AdaptiveLbOpt,
    pub route_config: RouteConfig,
    /// Optional virtual host this route belongs to (nginx `server_name`). `None` = default server.
    pub host: Option<String>,
    /// When true, `path` is a regex location matched against the full request path (default server
    /// only), tried after matchit routes (nginx `location ~ <regex>`).
    pub is_regex: bool,
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
        let backends = Self::build_backend_set(backends)?;

        Ok(Route {
            path,
            backends: Backends::new(Static::new(backends)),
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
        let backends = Self::build_backend_set(backends)?;

        Ok(Route {
            path: pattern.as_ref().to_string(),
            backends: Backends::new(Static::new(backends)),
            lb_options,
            route_config: RouteConfig::default(),
            host: None,
            is_regex: true,
        })
    }

    fn build_backend_set(
        backends: impl IntoIterator<Item = (String, usize)>,
    ) -> Result<BTreeSet<Backend>> {
        let backends = backends
            .into_iter()
            .map(|(addr, weight)| {
                let mut backend = Backend::new_with_weight(&addr, weight.max(1))
                    .expect("Invalid backend address");
                let http_peer = HttpPeer::new(backend.addr.clone(), false, String::new());
                backend.ext.insert(http_peer);
                backend
            })
            .collect::<BTreeSet<_>>();

        if backends.is_empty() {
            return Err(eyre!("Must provide at least one backend"));
        }
        Ok(backends)
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
    set_strategy_endpoint: Option<String>,
    server_config: Option<ServerConf>,
    tls: Option<TlsConfig>,
    prometheus_address: Option<String>,
    access_log: bool,
    https_redirect: bool,
    compression_level: u32,
    reload_config_path: Option<String>,
    request_id: bool,
    error_pages: HashMap<u16, String>,
}
impl ServerBuilder {
    pub fn add_route(mut self, route: impl Into<Route>) -> Self {
        self.routes.push(route.into());
        self
    }

    pub fn set_strategy_endpoint(mut self, addr: String) -> Self {
        self.set_strategy_endpoint = Some(addr);
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

    /// Reload per-route config from `path` on `SIGHUP` (nginx `-s reload`). Structural changes
    /// still require a restart.
    pub fn reload_on_sighup(mut self, path: String) -> Self {
        self.reload_config_path = Some(path);
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

    pub fn build(self) -> Server {
        assert!(!self.routes.is_empty(), "requires at least one route");
        // Use a caller-provided ServerConf as-is (it carries the configured tuning); otherwise fall
        // back to defaults with an aggressive upstream keepalive pool.
        let server_config = self.server_config.unwrap_or_else(|| {
            let mut config = ServerConf::default();
            config.upstream_keepalive_pool_size = 200000;
            config
        });
        let mut server = Server::new_with_opt_and_conf(None, server_config);

        let mut default_router = Router::new();
        let mut default_regex: Vec<(Regex, RouteValue)> = Vec::new();
        let mut vhost_routers: HashMap<String, Router<RouteValue>> = HashMap::new();
        let mut registry: RouteRegistry = HashMap::new();
        for route in self.routes {
            let lb_options = route.lb_options;

            let decision_engine = AdaptiveDecisionEngine::new(&lb_options);
            let lb = AdaptiveLoadBalancer::from_backends(
                route.backends,
                Some(lb_options),
                decision_engine,
            );

            let service_name = format!("adaptive-lb-{}", &route.path);
            let mut background_service = background_service(&service_name, lb);
            background_service.threads = Some(1);
            let task = background_service.task();
            server.add_service(background_service);

            let route_value = RouteValue {
                runtime: Arc::new(RouteRuntime::new(task, route.route_config)),
            };

            // Register the runtime so SIGHUP reload can target it (key mirrors RouteEntry::route_key).
            let host_key = route
                .host
                .as_deref()
                .map(|h| h.to_ascii_lowercase())
                .unwrap_or_default();
            registry.insert(
                (host_key, route.is_regex, route.path.clone()),
                route_value.runtime.clone(),
            );

            if route.is_regex {
                tracing::info!("Adding regex route: {}", route.path);
                let re = Regex::new(&route.path).expect("regex validated at construction");
                default_regex.push((re, route_value));
                continue;
            }

            match route.host {
                Some(host) => {
                    tracing::info!("Adding route: {} (host: {host})", route.path);
                    vhost_routers
                        .entry(host.to_ascii_lowercase())
                        .or_insert_with(Router::new)
                        .insert(route.path, route_value)
                        .expect("Invalid route");
                }
                None => {
                    tracing::info!("Adding route: {}", route.path);
                    default_router
                        .insert(route.path, route_value)
                        .expect("Invalid route");
                }
            }
        }
        let mut router = Proxy::with_regex_routes(
            default_router,
            default_regex,
            vhost_routers,
            DEFAULT_PATH_CACHE_CAPACITY,
        );
        router.set_access_log(self.access_log);
        router.set_https_redirect(self.https_redirect);
        router.set_compression_level(self.compression_level);
        router.set_request_id(self.request_id);
        router.set_error_pages(self.error_pages);

        if let Some(path) = self.reload_config_path {
            spawn_reload_watcher(path, Arc::new(registry));
        }

        if let Some(endpoint_address) = self.set_strategy_endpoint {
            let endpoint = SetStrategyEndpoint::service(router.clone(), &endpoint_address);
            server.add_service(endpoint);
        }

        let mut router_service = http_proxy_service(&server.configuration, router);
        let available_parallelism = thread::available_parallelism().map(Into::into).unwrap_or(1);
        router_service.threads = Some(available_parallelism);
        router_service.add_tcp(&self.address);

        if let Some(tls) = self.tls {
            let mut settings = TlsSettings::intermediate(&tls.cert_path, &tls.key_path)
                .expect("Failed to load TLS certificate/key");
            if tls.enable_h2 {
                settings.enable_h2();
            }
            router_service.add_tls_with_settings(&tls.address, None, settings);
            tracing::info!("TLS termination enabled on {}", tls.address);
        }

        server.add_service(router_service);

        let mut prometheus = Service::prometheus_http_service();
        let prometheus_address = self
            .prometheus_address
            .as_deref()
            .unwrap_or(PROMETHEUS_ENDPOINT_ADDRESS);
        prometheus.add_tcp(prometheus_address);
        server.add_service(prometheus);

        server.bootstrap();
        server
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
