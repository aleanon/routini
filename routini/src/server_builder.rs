use std::{
    net::{SocketAddr, TcpListener, ToSocketAddrs},
    sync::LazyLock,
};

use color_eyre::eyre::{Result, eyre};
use matchit::Router;
use pingora::{prelude::background_service, proxy::http_proxy_service, server::Server};
use regex::Regex;

use crate::{
    load_balancing::{LoadBalancer, health_check::TcpHealthCheck, strategy::Adaptive},
    proxy::{Proxy, RouteValue},
    set_strategy_endpoint::SetStrategyEndpoint,
    utils::constants::DEFAULT_MAX_ALGORITHM_ITERATIONS,
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
    pub backends: Vec<SocketAddr>,
    pub lb_strategy: Adaptive,
    pub include_health_check: bool,
    pub max_iterations: usize,
    pub route_config: RouteConfig,
}

impl Route {
    pub fn new<A: ToSocketAddrs>(
        path: impl AsRef<str>,
        backends: impl IntoIterator<Item = A>,
        lb_strategy: Adaptive,
    ) -> Result<Self> {
        if !PATH_REGEX.is_match(path.as_ref()) {
            return Err(eyre!(
                "Invalid path, it must start with '/', have at most one * and any eventual * must be at the end of the string"
            ));
        }
        let path = path.as_ref().replacen("*", "{*rest}", 1);

        let backends = backends.into_iter().try_fold(Vec::new(), |mut acc, a| {
            let addrs = a.to_socket_addrs()?;
            acc.extend(addrs);
            Ok::<_, color_eyre::Report>(acc)
        })?;
        if backends.is_empty() {
            return Err(eyre!("Must provide at least one backend"));
        }

        Ok(Route {
            path,
            backends,
            lb_strategy,
            include_health_check: true,
            max_iterations: DEFAULT_MAX_ALGORITHM_ITERATIONS,
            route_config: RouteConfig::default(),
        })
    }

    /// Default: [DEFAULT_MAX_ALGORITHM_ITERATIONS]
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Default: true
    pub fn include_health_check(mut self, include_health_check: bool) -> Self {
        self.include_health_check = include_health_check;
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

    pub fn build(self) -> Server {
        assert!(!self.routes.is_empty(), "requires at least one route");
        let mut server = Server::new(None).expect("Unable to create server instance");

        let mut routes = Router::new();
        for route in self.routes {
            let mut lb =
                LoadBalancer::try_from_iter_with_strategy(route.backends, route.lb_strategy)
                    .expect("Failed to parse backend addresses");

            if route.include_health_check {
                let hc = TcpHealthCheck::new();
                lb.set_health_check(hc);
            }

            let service_name = format!("lb updater-{}", &route.path);
            let background_service = background_service(&service_name, lb);
            let task = background_service.task();
            server.add_service(background_service);

            tracing::info!("Adding route: {}", route.path);

            let route_value = RouteValue {
                lb: task,
                max_iterations: route.max_iterations,
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
        router_service.add_tcp(&self.address);
        server.add_service(router_service);

        server.bootstrap();
        server
    }
}
