use std::sync::atomic::Ordering;

use pingora::{protocols::l4::socket::SocketAddr, upstreams::peer::Tracing};

use crate::least_connections::CONNECTIONS;

#[derive(Debug, Clone)]
pub struct ConnectionsTracer(pub SocketAddr);

impl Tracing for ConnectionsTracer {
    fn on_connected(&self) {
        CONNECTIONS
            .load()
            .get(&self.0)
            .and_then(|(_, count)| Some(count.fetch_add(1, Ordering::Relaxed)));
        log::debug!("incremented connection {}", &self.0)
    }

    fn on_disconnected(&self) {
        CONNECTIONS
            .load()
            .get(&self.0)
            .and_then(|(_, count)| Some(count.fetch_sub(1, Ordering::Relaxed)));
        log::debug!("decremented connection {}", &self.0);
    }

    fn boxed_clone(&self) -> Box<dyn Tracing> {
        Box::new(self.clone()) as Box<dyn Tracing>
    }
}
