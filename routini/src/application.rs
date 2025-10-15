use std::{collections::HashMap, fmt::Display, net::TcpListener, sync::Arc, time::Duration};

use pingora::{
    lb::{
        LoadBalancer,
        selection::{BackendIter, BackendSelection, Random, RoundRobin},
    },
    prelude::{TcpHealthCheck, background_service},
    proxy::http_proxy_service,
    server::Server,
};

use crate::{
    least_connections::LeastConnections,
    load_balancer::{
        DEFAULT_MAX_ALGORITHM_ITERATIONS, DynLoadBalancer, MultiLoadBalancer,
        MultiLoadBalancerHandle, RoutingConfig, StrategyId,
    },
};

pub struct Application {
    server: Server,
    router_handle: MultiLoadBalancerHandle,
}

pub enum StrategyKind {
    RoundRobin,
    Random,
    LeastConnections,
}

impl Display for StrategyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StrategyKind::RoundRobin => write!(f, "round_robin"),
            StrategyKind::Random => write!(f, "random"),
            StrategyKind::LeastConnections => write!(f, "least_connections"),
        }
    }
}

pub struct StrategyConfig {
    pub id: StrategyId,
    pub kind: StrategyKind,
}

impl StrategyConfig {
    pub fn new(kind: StrategyKind) -> Self {
        Self {
            id: kind.to_string(),
            kind,
        }
    }
}

impl Application {
    // It takes a listener instead of address string to be able to determine random port before creating the Application
    // for testing purposes
    pub fn new(
        listener: TcpListener,
        backends: impl IntoIterator<Item = String>,
        strategies: Vec<StrategyConfig>,
        initial_routing: RoutingConfig,
    ) -> Self {
        assert!(
            !strategies.is_empty(),
            "At least one strategy must be configured"
        );

        let backends: Vec<String> = backends.into_iter().collect();
        let mut server = Server::new(None).expect("Failed to create server");
        server.bootstrap();

        let mut strategy_map: HashMap<StrategyId, Arc<dyn DynLoadBalancer>> = HashMap::new();

        for StrategyConfig { id, kind } in strategies {
            let handle = match kind {
                StrategyKind::RoundRobin => {
                    register_strategy::<RoundRobin>(&mut server, &backends, &id, "round_robin")
                }
                StrategyKind::Random => {
                    register_strategy::<Random>(&mut server, &backends, &id, "random")
                }
                StrategyKind::LeastConnections => register_strategy::<LeastConnections>(
                    &mut server,
                    &backends,
                    &id,
                    "least_connections",
                ),
            };

            let existing = strategy_map.insert(id.clone(), handle);
            assert!(existing.is_none(), "duplicate strategy id: {id}");
        }

        assert!(
            strategy_map.contains_key(initial_routing.strategy()),
            "initial routing default must exist in strategies"
        );

        let router = MultiLoadBalancer::new(
            strategy_map,
            initial_routing,
            DEFAULT_MAX_ALGORITHM_ITERATIONS,
        );
        let handle = router.handle();
        let mut lb_service = http_proxy_service(&server.configuration, router);

        let socket_addr = listener
            .local_addr()
            .expect("Failed to get address from listener")
            .to_string();

        lb_service.add_tcp(&socket_addr);

        server.add_service(lb_service);

        Self {
            server,
            router_handle: handle,
        }
    }

    pub fn control(&self) -> MultiLoadBalancerHandle {
        self.router_handle.clone()
    }

    pub fn run(self) {
        self.server.run_forever()
    }
}

fn register_strategy<S>(
    server: &mut Server,
    backends: &[String],
    id: &str,
    name: &str,
) -> Arc<dyn DynLoadBalancer>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut lb = LoadBalancer::<S>::try_from_iter(backends.to_vec())
        .expect("Failed to create backends from addresses");

    let hc = TcpHealthCheck::new();
    lb.set_health_check(hc);
    lb.health_check_frequency = Some(Duration::from_secs(1));

    let background = background_service(&format!("health check ({}:{})", name, id), lb);
    let handle = background.task();
    server.add_service(background);

    handle as Arc<dyn DynLoadBalancer>
}
