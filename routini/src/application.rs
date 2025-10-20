use std::{net::TcpListener, time::Duration};

use pingora::{prelude::background_service, proxy::http_proxy_service, server::Server};
use serde::de::DeserializeOwned;

use crate::{
    load_balancing::{LoadBalancer, health_check::TcpHealthCheck, selection::Strategy},
    proxy::LB,
    set_strategy_endpoint::SetStrategyEndpoint,
    utils::constants::SET_STRATEGY_ENDPOINT_ADDRESS,
};

pub struct Application {
    server: Server,
}

impl Application {
    pub fn new<S>(
        listener: TcpListener,
        backends: impl IntoIterator<Item = String>,
        strategy: S,
    ) -> Self
    where
        S: Strategy + 'static + DeserializeOwned,
    {
        let mut server = Server::new(None).expect("Failed to create server");
        server.bootstrap();

        let mut lb = LoadBalancer::try_from_iter_with_strategy(backends, strategy)
            .expect("Failed to create backends from addresses");

        let hc = TcpHealthCheck::new();
        lb.set_health_check(hc);
        lb.health_check_frequency = Some(Duration::from_secs(1));

        let background = background_service("lb_updater", lb);
        let handle = background.task();
        server.add_service(background);

        let lb = LB {
            load_balancer: handle.clone(),
        };

        let update_strategy_endpoint =
            SetStrategyEndpoint::service(handle, SET_STRATEGY_ENDPOINT_ADDRESS);

        let mut lb_service = http_proxy_service(&server.configuration, lb);

        let socket_addr = listener
            .local_addr()
            .expect("Failed to get address from listener")
            .to_string();

        lb_service.add_tcp(&socket_addr);

        server.add_service(update_strategy_endpoint);
        server.add_service(lb_service);

        Self { server }
    }

    pub fn run(self) {
        self.server.run_forever()
    }
}
