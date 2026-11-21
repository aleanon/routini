use std::{str::FromStr, time::Duration};

use http::HeaderName;

use crate::{
    load_balancing::strategy::Adaptive,
    utils::constants::{
        DEFAULT_CONNECTIONS_DIV_RATIO, DEFAULT_DISCOVERY_INTERVAL,
        DEFAULT_EVALUATE_STRATEGY_FREQUENCY, DEFAULT_HEALTH_CHECK_FREQUENCY,
        DEFAULT_HYSTERESIS_EXIT_FACTOR, DEFAULT_LATENCY_DIV_RATIO,
        DEFAULT_MAX_ALGORITHM_ITERATIONS, DEFAULT_MIN_NR_OF_CONNECTIONS, DEFAULT_SMOOTHING_FACTOR,
    },
};

/// The Options to pass inn during construction of the load balancer.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptiveLbOpt {
    pub latency_smoothing_factor: f32,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    pub starting_strategy: Adaptive,
    pub evaluate_strategy_frequency: Duration,
    pub max_iterations: usize,
    pub health_check_interval: Option<Duration>,
    pub min_nr_of_connections: usize,
    /// Fraction of the enter thresholds used as exit thresholds (hysteresis).
    pub hysteresis_exit_factor: f32,
    /// What the hash strategies (FNVHash, Consistent) hash to pick a backend.
    pub hash_key: HashKey,
    /// How backends are probed, when `health_check_interval` is set.
    pub health_check: HealthCheckOpt,
    /// How often service discovery runs again, resolving upstream DNS names. `None`: once.
    pub discovery_interval: Option<Duration>,
}

/// An active health check (nginx Plus `health_check`): how a backend is probed and how many
/// results in a row flip its state.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthCheckOpt {
    pub probe: HealthProbe,
    /// Successful probes in a row that bring an unhealthy backend back (nginx `passes`).
    pub passes: usize,
    /// Failed probes in a row that take a healthy backend out (nginx `fails`).
    pub fails: usize,
}

impl Default for HealthCheckOpt {
    fn default() -> Self {
        Self {
            probe: HealthProbe::Tcp,
            passes: 1,
            fails: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HealthProbe {
    /// A backend is healthy if it accepts a TCP connection.
    Tcp,
    /// A backend is healthy if it answers a GET with an expected status.
    Http(HttpProbe),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpProbe {
    /// Path and query to request.
    pub path: String,
    /// `Host` header (and SNI unless `tls.sni` is set).
    pub host: String,
    /// Statuses that count as healthy. Empty: any 2xx or 3xx.
    pub expected_status: Vec<u16>,
    /// Connect and read timeout.
    pub timeout: Duration,
    /// Probe this port instead of the backend's.
    pub port: Option<u16>,
    /// Probe over TLS. Follows the route's upstream TLS settings.
    pub tls: Option<ProbeTls>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeTls {
    pub sni: String,
    pub verify: bool,
}

/// The part of a request the hash strategies select a backend with, so requests with the same
/// key go to the same backend (nginx `ip_hash`, `hash $request_uri`, `hash $cookie_x`). Other
/// strategies ignore it.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum HashKey {
    /// The client address (see [crate::real_ip]).
    #[default]
    ClientIp,
    /// Path and query.
    Uri,
    /// Path without the query.
    Path,
    /// A request header's value. Requests without it fall back to the client address.
    Header(HeaderName),
    /// A cookie's value. Requests without it fall back to the client address.
    Cookie(String),
}

impl FromStr for HashKey {
    type Err = String;

    /// `client_ip`, `uri`, `path`, `header:<name>` or `cookie:<name>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!(
                "invalid hash key '{s}', expected client_ip, uri, path, header:<name> or \
                 cookie:<name>"
            )
        };
        Ok(match s.split_once(':') {
            None => match s {
                "client_ip" => HashKey::ClientIp,
                "uri" => HashKey::Uri,
                "path" => HashKey::Path,
                _ => return Err(invalid()),
            },
            Some(("header", name)) => HashKey::Header(
                HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| invalid())?,
            ),
            Some(("cookie", name)) if valid_cookie_name(name.trim()) => {
                HashKey::Cookie(name.trim().to_string())
            }
            Some(_) => return Err(invalid()),
        })
    }
}

/// A cookie name is a non-empty token (RFC 6265).
fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&b))
}

impl Default for AdaptiveLbOpt {
    fn default() -> Self {
        Self {
            latency_smoothing_factor: DEFAULT_SMOOTHING_FACTOR,
            connections_divergence_ratio: DEFAULT_CONNECTIONS_DIV_RATIO,
            latency_divergence_ratio: DEFAULT_LATENCY_DIV_RATIO,
            starting_strategy: Adaptive::default(),
            evaluate_strategy_frequency: DEFAULT_EVALUATE_STRATEGY_FREQUENCY,
            max_iterations: DEFAULT_MAX_ALGORITHM_ITERATIONS,
            health_check_interval: Some(DEFAULT_HEALTH_CHECK_FREQUENCY),
            min_nr_of_connections: DEFAULT_MIN_NR_OF_CONNECTIONS,
            hysteresis_exit_factor: DEFAULT_HYSTERESIS_EXIT_FACTOR,
            hash_key: HashKey::default(),
            health_check: HealthCheckOpt::default(),
            discovery_interval: Some(DEFAULT_DISCOVERY_INTERVAL),
        }
    }
}

/// The stored configuration to be used at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptiveLbConfig {
    pub latency_smoothing_factor: f32,
    pub connections_divergence_ratio: f32,
    pub latency_divergence_ratio: f32,
    pub evaluate_strategy_frequency: Duration,
    pub max_iterations: usize,
    pub health_check_interval: Option<Duration>,
    pub min_nr_of_connections: usize,
    pub hysteresis_exit_factor: f32,
    pub hash_key: HashKey,
    pub discovery_interval: Option<Duration>,
}

impl From<AdaptiveLbOpt> for AdaptiveLbConfig {
    fn from(value: AdaptiveLbOpt) -> Self {
        Self::from(&value)
    }
}

impl From<&AdaptiveLbOpt> for AdaptiveLbConfig {
    fn from(value: &AdaptiveLbOpt) -> Self {
        Self {
            latency_smoothing_factor: value.latency_smoothing_factor,
            connections_divergence_ratio: value.connections_divergence_ratio,
            latency_divergence_ratio: value.latency_divergence_ratio,
            evaluate_strategy_frequency: value.evaluate_strategy_frequency,
            max_iterations: value.max_iterations,
            health_check_interval: value.health_check_interval,
            min_nr_of_connections: value.min_nr_of_connections,
            hysteresis_exit_factor: value.hysteresis_exit_factor,
            hash_key: value.hash_key.clone(),
            discovery_interval: value.discovery_interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_keys_parse() {
        assert_eq!("client_ip".parse(), Ok(HashKey::ClientIp));
        assert_eq!("uri".parse(), Ok(HashKey::Uri));
        assert_eq!("path".parse(), Ok(HashKey::Path));
        assert_eq!(
            "header:X-User-Id".parse(),
            Ok(HashKey::Header(HeaderName::from_static("x-user-id")))
        );
        assert_eq!(
            "cookie: session".parse(),
            Ok(HashKey::Cookie("session".into()))
        );
        for bad in [
            "ip",
            "header:",
            "header:bad name",
            "cookie:",
            "cookie:a=b",
            "query:x",
        ] {
            assert!(bad.parse::<HashKey>().is_err(), "{bad}");
        }
    }
}
