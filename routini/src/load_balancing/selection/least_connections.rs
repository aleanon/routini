use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use arc_swap::ArcSwap;
use pingora::{protocols::l4::socket::SocketAddr, upstreams::peer::Tracing};
use smallvec::SmallVec;

use crate::load_balancing::{
    Backend,
    selection::{BackendIter, BackendSelection},
};

type BackendIndex = usize;
type ConnectionCount = Arc<AtomicUsize>;

pub static CONNECTIONS: LazyLock<ArcSwap<BTreeMap<SocketAddr, (BackendIndex, ConnectionCount)>>> =
    LazyLock::new(|| ArcSwap::new(Arc::new(BTreeMap::new())));

pub struct LeastConnections {
    backends: Box<[Backend]>,
}

impl LeastConnections {
    pub fn new(backends: &BTreeSet<Backend>) -> Self {
        let backends = Vec::from_iter(backends.iter().cloned()).into_boxed_slice();
        Self::from_backends(backends)
    }

    // The pingora built in load balancer assumes stateless backend selection, The backend selection
    // is rebuilt every time update is called on the load balancer, this would cause connections to reset.
    // This is why a static is used here instead of holding the connections in the LeastConnections struct.
    pub fn from_backends(backends: Box<[Backend]>) -> Self {
        let connections = {
            let existing_connections = CONNECTIONS.load();
            backends
                .iter()
                .enumerate()
                .map(|(i, b)| match existing_connections.get(&b.addr) {
                    Some((_, conn_count)) => (b.addr.clone(), (i, conn_count.clone())),
                    None => (b.addr.clone(), (i, Arc::new(AtomicUsize::new(0)))),
                })
                .collect()
        };

        CONNECTIONS.store(Arc::new(connections));

        LeastConnections { backends }
    }
}

impl BackendSelection for LeastConnections {
    type Iter = LeastConnectionsIter;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        LeastConnections::new(backends)
    }

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        LeastConnectionsIter::new(self.clone(), key)
    }
}

pub struct LeastConnectionsIter {
    least_connections: Arc<LeastConnections>,
    /// The load balancer should use the first returned backend in the vast majority of cases,
    /// therefor we use a small vec to track previously returned backends. This lets us skip allocating
    /// most of the time when the iterator is used.
    yielded: SmallVec<[usize; 2]>,
}

impl LeastConnectionsIter {
    fn new(least_connections: Arc<LeastConnections>, _key: &[u8]) -> Self {
        Self {
            least_connections,
            yielded: SmallVec::new(),
        }
    }
}

impl BackendIter for LeastConnectionsIter {
    fn next(&mut self) -> Option<&Backend> {
        let conns = CONNECTIONS.load();
        let mut min_count = usize::MAX;
        let mut min_index = None;

        for (i, count) in conns.values() {
            if self.yielded.contains(i) {
                continue;
            }
            let c = count.load(Ordering::Relaxed);
            if c < min_count {
                min_count = c;
                min_index = Some(*i);
            }
        }

        if let Some(i) = min_index {
            self.yielded.push(i);
            self.least_connections.backends.get(i)
        } else {
            None
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn get_connection(least_connections: &Arc<LeastConnections>) -> Option<SocketAddr> {
        let mut iter = least_connections.iter(&[]);
        let Some(backend) = iter.next().and_then(|b| Some(b.addr.clone())) else {
            return None;
        };

        CONNECTIONS
            .load()
            .get(&backend)
            .unwrap()
            .1
            .fetch_add(1, Ordering::Relaxed);
        Some(backend)
    }

    #[test]
    fn test_next() {
        let backend1_addr = SocketAddr::Inet("127.0.0.1:8080".parse().unwrap());
        let backend2_addr = SocketAddr::Inet("127.0.0.1:8081".parse().unwrap());
        let backend3_addr = SocketAddr::Inet("127.0.0.1:8082".parse().unwrap());

        let backends = [&backend1_addr, &backend2_addr, &backend3_addr]
            .iter()
            .map(|a| Backend::new(&a.to_string()).unwrap())
            .collect();

        let least_connections = Arc::new(LeastConnections::new(&backends));
        backends.iter().enumerate().for_each(|(i, b)| {
            let map = CONNECTIONS.load();
            let connections = map.get(&b.addr).unwrap();
            connections.1.store(i, Ordering::Relaxed);
        });

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend1_addr.clone()
        );

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend1_addr.clone()
        );

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend2_addr.clone()
        );

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend1_addr.clone()
        );

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend2_addr.clone()
        );

        assert_eq!(
            get_connection(&least_connections).expect("Failed to connect to backend"),
            backend3_addr.clone()
        );
    }
}
