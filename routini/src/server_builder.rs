use std::{collections::BTreeSet, net::TcpListener, sync::LazyLock, thread, time::Duration};

use color_eyre::eyre::{Result, eyre};
use matchit::Router;
use pingora::{
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
    set_strategy_endpoint::SetStrategyEndpoint,
    utils::constants::{
        DEFAULT_HEALTH_CHECK_FREQUENCY, DEFAULT_MAX_ALGORITHM_ITERATIONS,
        DEFAULT_WILDCARD_IDENTIFIER, PROMETHEUS_ENDPOINT_ADDRESS,
    },
};

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
    }
}

pub struct RouteConfig {
    /// Strips the matching part of the path
    /// if we add a route /auth/*
    /// then we make a request to /auth/health,
    /// /auth will be stripped and the backend will receive the request
    /// with path /health
    pub strip_path_prefix: bool,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            strip_path_prefix: true,
        }
    }
}

pub struct Route {
    pub path: String,
    pub backends: Backends,
    pub lb_strategy: Adaptive,
    pub health_check_frequency: Option<Duration>,
    pub max_iterations: usize,
    pub route_config: RouteConfig,
}

impl Route {
    pub fn new<A: AsRef<str>>(
        path: impl AsRef<str>,
        backends: impl IntoIterator<Item = A>,
        lb_strategy: Adaptive,
    ) -> Result<Self> {
        if !PATH_REGEX.is_match(path.as_ref()) {
            return Err(eyre!(
                "Invalid path, it must start with '/', have at most one * and any eventual * must be at the end of the string"
            ));
        }
        let path = path.as_ref().replacen("*", DEFAULT_WILDCARD_IDENTIFIER, 1);

        let backends = backends
            .into_iter()
            .map(|addr| {
                let mut backend = Backend::new(addr.as_ref()).expect("Invalid backend address");
                let http_peer = HttpPeer::new(backend.addr.clone(), false, String::new());
                backend.ext.insert(http_peer);
                backend
            })
            .collect::<BTreeSet<_>>();

        if backends.is_empty() {
            return Err(eyre!("Must provide at least one backend"));
        }

        Ok(Route {
            path,
            backends: Backends::new(Static::new(backends)),
            lb_strategy,
            health_check_frequency: Some(DEFAULT_HEALTH_CHECK_FREQUENCY),
            max_iterations: DEFAULT_MAX_ALGORITHM_ITERATIONS,
            route_config: RouteConfig::default(),
        })
    }

    /// Default: [DEFAULT_MAX_ALGORITHM_ITERATIONS]
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Default: Some(Duration::from_secs(1))
    /// set to None to not run health check background service
    pub fn include_health_check(mut self, update_frequency: Option<Duration>) -> Self {
        self.health_check_frequency = update_frequency;
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

    pub fn build(self) -> Server {
        assert!(!self.routes.is_empty(), "requires at least one route");
        let mut server_config = self.server_config.unwrap_or(ServerConf::default());
        server_config.upstream_keepalive_pool_size = 100000;
        let mut server = Server::new_with_opt_and_conf(None, server_config);

        let mut routes = Router::new();
        for route in self.routes {
            let lb_options = AdaptiveLbOpt {
                max_iterations: route.max_iterations,
                starting_strategy: route.lb_strategy,
                health_check_interval: route.health_check_frequency,
                ..Default::default()
            };

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

            tracing::info!("Adding route: {}", route.path);

            let route_value = RouteValue {
                lb: task,
                route_config: route.route_config,
            };

            routes
                .insert(route.path, route_value)
                .expect("Invalid route");
        }
        let router = Proxy::new(routes);

        if let Some(endpoint_address) = self.set_strategy_endpoint {
            let endpoint = SetStrategyEndpoint::service(router.clone(), &endpoint_address);
            server.add_service(endpoint);
        }

        let mut router_service = http_proxy_service(&server.configuration, router);
        let available_parallelism = thread::available_parallelism().map(Into::into).unwrap_or(1);
        router_service.threads = Some(available_parallelism);
        router_service.add_tcp(&self.address);
        server.add_service(router_service);

        let mut prometheus = Service::prometheus_http_service();
        prometheus.add_tcp(PROMETHEUS_ENDPOINT_ADDRESS);
        server.add_service(prometheus);

        server.bootstrap();
        server
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_route_valid_simple_path() {
        let route = Route::new("/api", vec!["127.0.0.1:8080"], Adaptive::default());
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.path, "/api");

        // Initialize backends from discovery
        let strategy = route.lb_strategy.clone();
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
        let strategy = route.lb_strategy.clone();
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
            });

        assert_eq!(route.max_iterations, 20);
        assert_eq!(route.health_check_frequency, None);
        assert_eq!(route.route_config.strip_path_prefix, false);
    }

    #[test]
    fn test_route_default_values() {
        let route = Route::new("/api", vec!["127.0.0.1:8080"], Adaptive::default()).unwrap();

        assert_eq!(route.max_iterations, DEFAULT_MAX_ALGORITHM_ITERATIONS);
        assert_eq!(route.health_check_frequency, Some(Duration::from_secs(1)));
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
