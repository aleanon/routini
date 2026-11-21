//! The client address behind trusted proxies (nginx `set_real_ip_from`, `real_ip_header`,
//! `real_ip_recursive on`).
//!
//! Behind a load balancer every connection comes from the load balancer, so per-IP rate limits,
//! allow/deny rules, the hash key and the access log would all see one address. When the
//! connecting peer is a trusted proxy, the client address is taken from a request header the
//! proxy set instead.

use std::net::{IpAddr, SocketAddr};

use http::{HeaderMap, HeaderName};
use ipnet::IpNet;

#[derive(Debug, Clone)]
pub struct RealIp {
    /// Proxies whose header is believed. Requests from anywhere else keep their peer address.
    pub trusted: Vec<IpNet>,
    /// The header holding the client address, a comma separated list like `X-Forwarded-For`
    /// or a single address like `X-Real-IP`.
    pub header: HeaderName,
}

impl RealIp {
    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&ip))
    }

    /// The client address of a request from `peer`, `None` if it can't be told.
    ///
    /// Only a trusted `peer` is believed. Its header is read right to left, skipping trusted
    /// addresses, since every proxy appends the address it saw: the first untrusted address is
    /// the client. Entries left of it were written by the client and are ignored. If every
    /// address is trusted the leftmost one is used, and without the header the peer itself is
    /// the client (e.g. a load balancer's health check).
    ///
    /// An entry that isn't an address, or a line that isn't visible ASCII, ends the walk: what
    /// lies left of it can't be trusted. If no untrusted address was reached by then the client
    /// is unknown and the proxy rejects the request. Falling back to a trusted proxy's address
    /// instead would let a client pass allow rules meant for the internal network, or use up
    /// the proxy's own rate limit, by sending garbage.
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> Option<IpAddr> {
        let peer = peer.to_canonical();
        if !self.is_trusted(peer) {
            return Some(peer);
        }
        let mut client = peer;
        // Several header lines are one list, in order
        for line in headers.get_all(&self.header).iter().rev() {
            let line = line.to_str().ok()?;
            for entry in line.rsplit(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                client = parse_addr(entry)?;
                if !self.is_trusted(client) {
                    return Some(client);
                }
            }
        }
        Some(client)
    }
}

/// An address as proxies write it: `1.2.3.4`, `1.2.3.4:5678`, `::1` or `[::1]:5678`.
fn parse_addr(entry: &str) -> Option<IpAddr> {
    entry
        .parse::<IpAddr>()
        .or_else(|_| entry.parse::<SocketAddr>().map(|s| s.ip()))
        .ok()
        .map(|ip| ip.to_canonical())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_ip(trusted: &[&str]) -> RealIp {
        RealIp {
            trusted: trusted.iter().map(|n| n.parse().unwrap()).collect(),
            header: HeaderName::from_static("x-forwarded-for"),
        }
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append("x-forwarded-for", v.parse().unwrap());
        }
        map
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_keeps_its_address_and_header_is_ignored() {
        let r = real_ip(&["10.0.0.0/8"]);
        assert_eq!(
            r.resolve(ip("203.0.113.9"), &headers(&["198.51.100.1"])),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn trusted_peer_yields_the_rightmost_untrusted_address() {
        let r = real_ip(&["10.0.0.0/8"]);
        // The client prepended a fake address; the last proxy hop appended the real one
        let h = headers(&["6.6.6.6, 198.51.100.1, 10.0.0.2"]);
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), Some(ip("198.51.100.1")));
    }

    #[test]
    fn multiple_header_lines_form_one_list() {
        let r = real_ip(&["10.0.0.0/8"]);
        let h = headers(&["6.6.6.6", "198.51.100.1, 10.0.0.2"]);
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), Some(ip("198.51.100.1")));
        let h = headers(&["198.51.100.1", "10.0.0.2"]);
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), Some(ip("198.51.100.1")));
    }

    #[test]
    fn all_trusted_or_missing_header() {
        let r = real_ip(&["10.0.0.0/8"]);
        assert_eq!(
            r.resolve(ip("10.0.0.1"), &headers(&["10.0.0.3, 10.0.0.2"])),
            Some(ip("10.0.0.3"))
        );
        assert_eq!(r.resolve(ip("10.0.0.1"), &HeaderMap::new()), Some(ip("10.0.0.1")));
    }

    #[test]
    fn garbage_stops_the_walk() {
        let r = real_ip(&["10.0.0.0/8"]);
        // An untrusted address right of the garbage is the client
        let h = headers(&["unknown, 198.51.100.1, 10.0.0.2"]);
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), Some(ip("198.51.100.1")));
        // Garbage before reaching one: unknown, not the trusted hop next to it
        let h = headers(&["198.51.100.1, unknown, 10.0.0.2"]);
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), None);
    }

    #[test]
    fn unreadable_line_stops_the_walk() {
        let r = real_ip(&["10.0.0.0/8"]);
        // The client sent an allowed address and a line with a non-ASCII byte; a proxy appended
        // the real address to that last line, making it unreadable as a whole
        let mut h = headers(&["203.0.113.50"]);
        h.append(
            "x-forwarded-for",
            http::HeaderValue::from_bytes(b"\xff, 198.51.100.1").unwrap(),
        );
        assert_eq!(r.resolve(ip("10.0.0.1"), &h), None);
    }

    #[test]
    fn ports_brackets_and_mapped_ipv6_are_understood() {
        let r = real_ip(&["10.0.0.0/8"]);
        assert_eq!(
            r.resolve(ip("::ffff:10.0.0.1"), &headers(&["198.51.100.1:4711"])),
            Some(ip("198.51.100.1"))
        );
        assert_eq!(
            r.resolve(ip("10.0.0.1"), &headers(&["[2001:db8::1]:443"])),
            Some(ip("2001:db8::1"))
        );
    }

    #[test]
    fn x_real_ip_style_single_value() {
        let r = RealIp {
            header: HeaderName::from_static("x-real-ip"),
            ..real_ip(&["127.0.0.1/32"])
        };
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", "198.51.100.7".parse().unwrap());
        assert_eq!(r.resolve(ip("127.0.0.1"), &h), Some(ip("198.51.100.7")));
    }
}
