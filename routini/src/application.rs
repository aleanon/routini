use std::{marker::PhantomData, net::TcpListener};

use pingora::{
    lb::{
        LoadBalancer,
        selection::{BackendIter, BackendSelection},
    },
    prelude::{TcpHealthCheck, background_service},
    proxy::http_proxy_service,
    server::Server,
};

use crate::load_balancer::LB;

pub struct Application<A>
where
    A: BackendSelection + 'static + Send + Sync,
    A::Iter: BackendIter,
{
    server: Server,
    _selection_algorithm: PhantomData<A>,
}

impl<A> Application<A>
where
    A: BackendSelection + 'static + Send + Sync,
    A::Iter: BackendIter,
{
    //TODO: Make the new function take listeners instead of address strings, need to create the backends manually
    // It takes a listener instead of address string to be able to determine random port before creating the Application
    // for testing purposes
    pub fn new(listener: TcpListener, backends: impl IntoIterator<Item = String>) -> Self {
        let mut server = Server::new(None).unwrap();
        server.bootstrap();

        let mut upstreams = LoadBalancer::<A>::try_from_iter(backends).unwrap();

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
            .expect("Failed to get address from listener")
            .to_string();

        lb_service.add_tcp(&socket_addr);

        server.add_service(lb_service);
        server.add_service(background);

        Self {
            server,
            _selection_algorithm: PhantomData,
        }
    }

    pub fn run(self) {
        self.server.run_forever()
    }
}
