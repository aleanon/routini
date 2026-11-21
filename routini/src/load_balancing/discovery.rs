// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Service discovery interface and implementations

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::Result;
use pingora::lb::Extensions;
use pingora::prelude::HttpPeer;
use pingora::protocols::l4::socket::SocketAddr;
use std::io::Result as IoResult;
use std::net::ToSocketAddrs;
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use crate::load_balancing::{Backend, Metrics, NoMetric};

/// [ServiceDiscovery] is the interface to discover [Backend]s.
#[async_trait]
pub trait ServiceDiscovery<M: Metrics = NoMetric> {
    /// Return the discovered collection of backends.
    /// And *optionally* whether these backends are enabled to serve or not in a `HashMap`. Any backend
    /// that is not explicitly in the set is considered enabled.
    async fn discover(&self) -> Result<(BTreeSet<Backend<M>>, HashMap<u64, bool>)>;
}

/// Lets a caller keep a handle to the discovery it hands to [super::Backends], e.g. to change a
/// [Static] set on config reload.
#[async_trait]
impl<M: Metrics, T: ServiceDiscovery<M> + Send + Sync + ?Sized> ServiceDiscovery<M> for Arc<T> {
    async fn discover(&self) -> Result<(BTreeSet<Backend<M>>, HashMap<u64, bool>)> {
        (**self).discover().await
    }
}

/// A static collection of [Backend]s for service discovery.
#[derive(Default)]
pub struct Static<M: Metrics = NoMetric> {
    backends: ArcSwap<BTreeSet<Backend<M>>>,
}

impl<M: Metrics> Static<M> {
    /// Create a new boxed [Static] service discovery with the given backends.
    pub fn new(backends: BTreeSet<Backend<M>>) -> Box<Self> {
        Box::new(Static {
            backends: ArcSwap::new(Arc::new(backends)),
        })
    }

    /// Create a new boxed [Static] from a given iterator of items that implements [ToSocketAddrs].
    pub fn try_from_iter<A, T: IntoIterator<Item = A>>(iter: T) -> IoResult<Box<Self>>
    where
        A: ToSocketAddrs,
    {
        let mut upstreams = BTreeSet::new();
        for addrs in iter.into_iter() {
            let addrs = addrs.to_socket_addrs()?.map(|addr| {
                let addr = SocketAddr::Inet(addr);
                Backend {
                    peer: HttpPeer::new(addr.clone(), false, String::new()),
                    addr,
                    weight: 1,
                    ext: Extensions::new(),
                    metrics: M::default(),
                }
            });
            upstreams.extend(addrs);
        }
        Ok(Self::new(upstreams))
    }

    /// return the collection to backends
    pub fn get(&self) -> BTreeSet<Backend<M>> {
        BTreeSet::clone(&self.backends.load())
    }

    // Concurrent set/add/remove might race with each other
    // TODO: use a queue to avoid racing

    // TODO: take an impl iter
    pub(crate) fn set(&self, backends: BTreeSet<Backend<M>>) {
        self.backends.store(backends.into())
    }

    #[allow(dead_code)]
    pub(crate) fn add(&self, backend: Backend<M>) {
        let mut new = self.get();
        new.insert(backend);
        self.set(new)
    }

    #[allow(dead_code)]
    pub(crate) fn remove(&self, backend: &Backend<M>) {
        let mut new = self.get();
        new.remove(backend);
        self.set(new)
    }
}

/// A configured upstream: an IP address or a DNS name, a port and a weight.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub weight: usize,
}

impl Upstream {
    /// Parse `1.2.3.4:80`, `[::1]:80` or `name:80`.
    pub fn parse(address: &str, weight: usize) -> std::result::Result<Self, String> {
        if let Ok(addr) = address.parse::<std::net::SocketAddr>() {
            return Ok(Self {
                host: addr.ip().to_string(),
                port: addr.port(),
                weight,
            });
        }
        let invalid = || format!("expected ip:port or hostname:port, got '{address}'");
        let (host, port) = address.rsplit_once(':').ok_or_else(invalid)?;
        let port = port.parse().map_err(|_| invalid())?;
        let valid_name = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        if !valid_name {
            return Err(invalid());
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            weight,
        })
    }

    fn ip(&self) -> Option<std::net::IpAddr> {
        self.host.parse().ok()
    }

    /// Whether the host is a DNS name that has to be resolved.
    pub fn is_name(&self) -> bool {
        self.ip().is_none()
    }
}

/// How long a single DNS lookup may take before the previous result is used.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Service discovery for configured [Upstream]s: addresses are used as they are, DNS names are
/// resolved on every [discover](ServiceDiscovery::discover) (nginx `server name resolve`), each
/// address they resolve to becoming a backend. A lookup that fails or returns nothing keeps the
/// name's previous addresses, so a DNS outage doesn't empty the pool.
pub struct UpstreamDiscovery<M: Metrics = NoMetric> {
    upstreams: ArcSwap<Vec<Upstream>>,
    /// The last successful lookup per (name, port).
    resolved: std::sync::Mutex<HashMap<(String, u16), Vec<std::net::SocketAddr>>>,
    _metrics: std::marker::PhantomData<fn() -> M>,
}

impl<M: Metrics> UpstreamDiscovery<M> {
    pub fn new(upstreams: Vec<Upstream>) -> Self {
        Self {
            upstreams: ArcSwap::from_pointee(upstreams),
            resolved: Default::default(),
            _metrics: std::marker::PhantomData,
        }
    }

    pub fn get(&self) -> Vec<Upstream> {
        Vec::clone(&self.upstreams.load())
    }

    /// Replace the upstreams, e.g. on config reload. Takes effect on the next discovery.
    pub fn set(&self, upstreams: Vec<Upstream>) {
        self.resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(host, port), _| upstreams.iter().any(|u| &u.host == host && u.port == *port));
        self.upstreams.store(Arc::new(upstreams));
    }

    async fn addresses(&self, upstream: &Upstream) -> Vec<std::net::SocketAddr> {
        if let Some(ip) = upstream.ip() {
            return vec![std::net::SocketAddr::new(ip, upstream.port)];
        }
        let key = (upstream.host.clone(), upstream.port);
        let lookup = tokio::net::lookup_host((upstream.host.as_str(), upstream.port));
        let result = match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
            Ok(Ok(addrs)) => {
                let mut addrs: Vec<_> = addrs.collect();
                addrs.sort();
                addrs.dedup();
                (!addrs.is_empty())
                    .then_some(addrs)
                    .ok_or_else(|| "no addresses".to_string())
            }
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err(format!("timed out after {LOOKUP_TIMEOUT:?}")),
        };
        let mut resolved = self.resolved.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(addrs) => {
                resolved.insert(key, addrs.clone());
                addrs
            }
            Err(err) => {
                let previous = resolved.get(&key).cloned().unwrap_or_default();
                tracing::warn!(
                    upstream = %upstream.host,
                    "DNS lookup failed ({err}), keeping {} previous address(es)",
                    previous.len()
                );
                previous
            }
        }
    }
}

#[async_trait]
impl<M: Metrics> ServiceDiscovery<M> for UpstreamDiscovery<M> {
    async fn discover(&self) -> Result<(BTreeSet<Backend<M>>, HashMap<u64, bool>)> {
        let upstreams = self.upstreams.load_full();
        let mut backends = BTreeSet::new();
        for upstream in upstreams.iter() {
            for addr in self.addresses(upstream).await {
                backends.insert(Backend::from_socket_addr(addr, upstream.weight.max(1)));
            }
        }
        Ok((backends, HashMap::new()))
    }
}

#[async_trait]
impl<M: Metrics> ServiceDiscovery<M> for Static<M> {
    async fn discover(&self) -> Result<(BTreeSet<Backend<M>>, HashMap<u64, bool>)> {
        // no readiness
        let health = HashMap::new();
        Ok((self.get(), health))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstreams_parse_addresses_and_names() {
        let up = |a: &str| Upstream::parse(a, 1);
        assert_eq!(up("10.0.0.1:80").unwrap().host, "10.0.0.1");
        assert!(!up("[::1]:8080").unwrap().is_name());
        let named = up("API.internal:8080").unwrap();
        assert_eq!((named.host.as_str(), named.port), ("api.internal", 8080));
        assert!(named.is_name());
        assert!(up("my_svc.default.svc:80").unwrap().is_name());
        for bad in [
            "10.0.0.1",
            "name",
            "name:",
            "name:99999",
            ":80",
            "a b:80",
            "a..b:80",
            "x/y:80",
        ] {
            assert!(up(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn names_are_resolved_and_failures_keep_the_previous_addresses() {
        let d = UpstreamDiscovery::<NoMetric>::new(vec![
            Upstream::parse("localhost:4000", 2).unwrap(),
            Upstream::parse("10.0.0.1:80", 1).unwrap(),
        ]);
        let (backends, _) = d.discover().await.unwrap();
        let addrs: Vec<String> = backends.iter().map(|b| b.addr.to_string()).collect();
        assert!(addrs.contains(&"127.0.0.1:4000".to_string()), "{addrs:?}");
        assert!(addrs.contains(&"10.0.0.1:80".to_string()));
        assert!(backends.iter().all(|b| b.weight
            == if b.addr.to_string() == "10.0.0.1:80" {
                1
            } else {
                2
            }));

        // Pretend the last lookup of a name that no longer resolves succeeded
        d.set(vec![
            Upstream::parse("does-not-exist.invalid:80", 1).unwrap(),
        ]);
        let cached = "192.0.2.9:80".parse().unwrap();
        d.resolved
            .lock()
            .unwrap()
            .insert(("does-not-exist.invalid".into(), 80), vec![cached]);
        let (backends, _) = d.discover().await.unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(
            backends.iter().next().unwrap().addr.to_string(),
            "192.0.2.9:80"
        );
        // Removed names are forgotten
        d.set(vec![]);
        assert!(d.resolved.lock().unwrap().is_empty());
    }
}
