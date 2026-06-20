//! Per-route runtime configuration shared by the proxy's request/response filters.
//!
//! [`RouteConfig`] holds everything configurable per route (header rules, etc.). [`RouteRuntime`]
//! bundles a route's config with its load balancer and is what the proxy stores per route and
//! caches per concrete request path.
use std::{net::IpAddr, sync::Arc, time::Duration};

use http::{HeaderName, HeaderValue, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::HttpPeer;

use crate::adaptive_loadbalancer::{AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine};

pub type SharedLb = Arc<AdaptiveLoadBalancer<AdaptiveDecisionEngine>>;

/// Upstream connection/transfer timeouts, applied to the [`HttpPeer`] per request.
/// `None` leaves Pingora's default in place. Equivalent to nginx `proxy_connect_timeout`,
/// `proxy_read_timeout`, `proxy_send_timeout`.
#[derive(Debug, Clone, Default)]
pub struct TimeoutConfig {
    pub connect: Option<Duration>,
    pub read: Option<Duration>,
    pub write: Option<Duration>,
    pub idle: Option<Duration>,
}

impl TimeoutConfig {
    /// Overlay any configured timeouts onto a peer's options.
    pub fn apply(&self, peer: &mut HttpPeer) {
        if let Some(connect) = self.connect {
            peer.options.connection_timeout = Some(connect);
        }
        if let Some(read) = self.read {
            peer.options.read_timeout = Some(read);
        }
        if let Some(write) = self.write {
            peer.options.write_timeout = Some(write);
        }
        if let Some(idle) = self.idle {
            peer.options.idle_timeout = Some(idle);
        }
    }
}

/// Policy for the `Host` header sent to the upstream.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum HostRewrite {
    /// Forward the client's original `Host` header unchanged (nginx `proxy_set_header Host $host`).
    #[default]
    Preserve,
    /// Replace the `Host` header with a fixed value.
    Set(String),
}

/// Request/response header manipulation rules, the equivalent of nginx's `proxy_set_header`,
/// `add_header`, and the forwarded-header conventions.
#[derive(Debug, Clone)]
pub struct HeaderRules {
    /// Add `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Real-IP`.
    pub forwarded: bool,
    /// When `true`, append the client IP to an existing `X-Forwarded-For` (we are behind a trusted
    /// proxy). When `false`, reset `X-Forwarded-For` to just the direct client IP to avoid spoofing.
    pub trusted_proxy: bool,
    pub host: HostRewrite,
    pub set_request: Vec<(HeaderName, HeaderValue)>,
    pub remove_request: Vec<HeaderName>,
    pub add_response: Vec<(HeaderName, HeaderValue)>,
    pub remove_response: Vec<HeaderName>,
}

impl Default for HeaderRules {
    fn default() -> Self {
        Self {
            // A reverse proxy should advertise the real client by default.
            forwarded: true,
            trusted_proxy: false,
            host: HostRewrite::Preserve,
            set_request: Vec::new(),
            remove_request: Vec::new(),
            add_response: Vec::new(),
            remove_response: Vec::new(),
        }
    }
}

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");

impl HeaderRules {
    /// Apply request-header rules to the outgoing upstream request.
    ///
    /// `client_ip`/`client_tls` describe the downstream connection and drive the forwarded headers.
    pub fn apply_request(
        &self,
        upstream: &mut RequestHeader,
        client_ip: Option<IpAddr>,
        client_tls: bool,
    ) {
        if let HostRewrite::Set(value) = &self.host {
            if let Ok(v) = HeaderValue::from_str(value) {
                let _ = upstream.insert_header(header::HOST, v);
            }
        }

        if self.forwarded {
            if let Some(ip) = client_ip {
                let ip_str = ip.to_string();
                let _ = upstream.insert_header(X_REAL_IP, &ip_str);

                let xff = if self.trusted_proxy {
                    match upstream.headers.get(&X_FORWARDED_FOR) {
                        Some(existing) => format!(
                            "{}, {ip_str}",
                            existing.to_str().unwrap_or_default()
                        ),
                        None => ip_str.clone(),
                    }
                } else {
                    ip_str
                };
                let _ = upstream.insert_header(X_FORWARDED_FOR, &xff);

                let proto = if client_tls { "https" } else { "http" };
                let _ = upstream.insert_header(X_FORWARDED_PROTO, proto);
            }
        }

        for (name, value) in &self.set_request {
            let _ = upstream.insert_header(name.clone(), value.clone());
        }
        for name in &self.remove_request {
            upstream.remove_header(name);
        }
    }

    /// Apply response-header rules to the downstream response.
    pub fn apply_response(&self, response: &mut ResponseHeader) {
        for (name, value) in &self.add_response {
            let _ = response.insert_header(name.clone(), value.clone());
        }
        for name in &self.remove_response {
            response.remove_header(name);
        }
    }
}

/// All per-route configuration. Built once (from the builder or config file) and shared read-only.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// Strip the matched route prefix before forwarding (see the route docs).
    pub strip_path_prefix: bool,
    pub headers: HeaderRules,
    pub timeouts: TimeoutConfig,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            strip_path_prefix: true,
            headers: HeaderRules::default(),
            timeouts: TimeoutConfig::default(),
        }
    }
}

/// A route's config bundled with its load balancer. Shared via `Arc` and never mutated after build.
pub struct RouteRuntime {
    pub lb: SharedLb,
    pub config: RouteConfig,
}

impl RouteRuntime {
    pub fn new(lb: SharedLb, config: RouteConfig) -> Self {
        Self { lb, config }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> RequestHeader {
        RequestHeader::build("GET", b"/", None).unwrap()
    }

    #[test]
    fn forwarded_headers_set_for_direct_client() {
        let rules = HeaderRules::default();
        let mut r = req();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), true);

        assert_eq!(r.headers.get(&X_REAL_IP).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_PROTO).unwrap(), "https");
    }

    #[test]
    fn untrusted_proxy_resets_xff() {
        let rules = HeaderRules::default(); // trusted_proxy = false
        let mut r = req();
        r.insert_header(X_FORWARDED_FOR, "1.2.3.4").unwrap();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), false);

        // spoofed upstream value is discarded, only the real client IP remains
        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "203.0.113.5");
        assert_eq!(r.headers.get(&X_FORWARDED_PROTO).unwrap(), "http");
    }

    #[test]
    fn trusted_proxy_appends_xff() {
        let rules = HeaderRules {
            trusted_proxy: true,
            ..Default::default()
        };
        let mut r = req();
        r.insert_header(X_FORWARDED_FOR, "1.2.3.4").unwrap();
        rules.apply_request(&mut r, Some("203.0.113.5".parse().unwrap()), false);

        assert_eq!(r.headers.get(&X_FORWARDED_FOR).unwrap(), "1.2.3.4, 203.0.113.5");
    }

    #[test]
    fn host_rewrite_and_custom_headers() {
        let rules = HeaderRules {
            forwarded: false,
            host: HostRewrite::Set("upstream.internal".into()),
            set_request: vec![(
                HeaderName::from_static("x-api"),
                HeaderValue::from_static("1"),
            )],
            remove_request: vec![HeaderName::from_static("cookie")],
            ..Default::default()
        };
        let mut r = req();
        r.insert_header(http::header::COOKIE, "secret").unwrap();
        rules.apply_request(&mut r, None, false);

        assert_eq!(r.headers.get(http::header::HOST).unwrap(), "upstream.internal");
        assert_eq!(r.headers.get("x-api").unwrap(), "1");
        assert!(r.headers.get(http::header::COOKIE).is_none());
    }

    #[test]
    fn timeouts_overlay_peer_options() {
        let cfg = TimeoutConfig {
            connect: Some(Duration::from_millis(500)),
            read: Some(Duration::from_secs(5)),
            write: None,
            idle: None,
        };
        let mut peer = HttpPeer::new("127.0.0.1:8080", false, String::new());
        let default_write = peer.options.write_timeout;
        cfg.apply(&mut peer);

        assert_eq!(peer.options.connection_timeout, Some(Duration::from_millis(500)));
        assert_eq!(peer.options.read_timeout, Some(Duration::from_secs(5)));
        // unset fields are left untouched
        assert_eq!(peer.options.write_timeout, default_write);
    }

    #[test]
    fn response_headers_added_and_removed() {
        let rules = HeaderRules {
            add_response: vec![(
                HeaderName::from_static("x-served-by"),
                HeaderValue::from_static("routini"),
            )],
            remove_response: vec![HeaderName::from_static("server")],
            ..Default::default()
        };
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header(http::header::SERVER, "secret-server").unwrap();
        rules.apply_response(&mut resp);

        assert_eq!(resp.headers.get("x-served-by").unwrap(), "routini");
        assert!(resp.headers.get(http::header::SERVER).is_none());
    }
}
