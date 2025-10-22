use std::net::TcpListener;

use matchit::Router;
use pingora::{
    prelude::{Opt, background_service},
    proxy::http_proxy_service,
    server::{Server, configuration::ServerConf},
};

use crate::{
    load_balancing::{LoadBalancer, health_check::TcpHealthCheck, strategy::Adaptive},
    proxy::{Proxy, RouteValue},
    set_strategy_endpoint::SetStrategyEndpoint,
};

pub fn application(listener: TcpListener) -> ApplicationBuilder {
    let address = listener
        .local_addr()
        .expect("Invalid socket address")
        .to_string();

    ApplicationBuilder {
        address,
        routes: Vec::new(),
        set_strategy_endpoint: None,
        server_config: None,
        server_options: None,
    }
}

pub struct RouteConfig {
    /// Strips the matching part of the path
    /// if we add a route /auth/{*rest}
    /// then we make a request to /auth/health,
    /// /auth will be stripped and the backend will receive the request
    /// with path /health
    pub strip_path_prefix: bool,
}

pub struct Route {
    pub path: String,
    pub backends: Vec<String>,
    pub lb_strategy: Adaptive,
    pub include_health_check: bool,
    pub max_iterations: usize,
    pub route_config: RouteConfig,
}

impl Route {
    pub fn new(
        path: impl ToString,
        backends: impl IntoIterator<Item = String>,
        lb_strategy: Adaptive,
        include_health_check: bool,
        max_iterations: usize,
        route_config: RouteConfig,
    ) -> Self {
        Route {
            path: path.to_string(),
            backends: backends.into_iter().collect(),
            lb_strategy,
            include_health_check,
            max_iterations,
            route_config,
        }
    }
}

pub struct ApplicationBuilder {
    address: String,
    server_options: Option<Opt>,
    server_config: Option<ServerConf>,
    routes: Vec<Route>,
    set_strategy_endpoint: Option<String>,
}
impl ApplicationBuilder {
    pub fn add_route(mut self, route: impl Into<Route>) -> Self {
        self.routes.push(route.into());
        self
    }

    pub fn options(mut self, options: Opt) -> Self {
        self.server_options = Some(options);
        self
    }

    pub fn server_config(mut self, config: ServerConf) -> Self {
        self.server_config = Some(config);
        self
    }

    pub fn set_strategy_endpoint(mut self, addr: String) -> Self {
        self.set_strategy_endpoint = Some(addr);
        self
    }

    pub fn build(self) -> Server {
        assert!(!self.routes.is_empty(), "requires at least one route");
        let mut server = Server::new_with_opt_and_conf(
            self.server_options,
            self.server_config.unwrap_or_default(),
        );

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

pub struct Application(Server);

impl Application {
    pub fn run(self) {
        self.0.run_forever()
    }
}
