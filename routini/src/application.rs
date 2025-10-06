use std::net::TcpListener;

use pingora::{
    lb::LoadBalancer,
    prelude::{TcpHealthCheck, background_service},
    proxy::http_proxy_service,
    server::Server,
};

use crate::load_balancer::LB;

pub struct Application {
    server: Server,
}

impl Application {
    //TODO: Make the new function take listeners instead of address strings, need to create the backends manually
    pub fn new(listener: TcpListener, backends: impl IntoIterator<Item = String>) -> Self {
        let mut server = Server::new(None).unwrap();
        server.bootstrap();

        let mut upstreams = LoadBalancer::try_from_iter(backends).unwrap();

        let hc = TcpHealthCheck::new();
        upstreams.set_health_check(hc);
        upstreams.health_check_frequency = Some(std::time::Duration::from_secs(1));

        let background = background_service("health check", upstreams);
        let upstreams = background.task();

        let lb = LB {
            backends: upstreams,
        };

        let mut lb_service = http_proxy_service(&server.configuration, lb);

        let socket_addr = listener
            .local_addr()
            .expect("Failed to get address from listener");
        let addr = socket_addr.ip();
        let port = socket_addr.port();
        lb_service.add_tcp(&format!("{}:{}", addr, port));

        server.add_service(lb_service);
        server.add_service(background);

        Self { server }
    }

    pub fn run(self) {
        self.server.run_forever()
    }
}
