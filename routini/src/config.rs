//! Declarative configuration for the proxy, deserialized from `config.json`.
//!
//! The shapes here mirror `config.json` and convert into the builder types in
//! [`crate::server_builder`], so `main` can construct the whole server from a file instead of
//! hard-coded values.
use std::{collections::HashMap, net::IpAddr, time::Duration};

use base64::prelude::{BASE64_STANDARD, Engine};
use color_eyre::eyre::{Result, WrapErr};
use http::{HeaderName, HeaderValue};
use ipnet::IpNet;
use serde::Deserialize;

use crate::{
    adaptive_loadbalancer::options::AdaptiveLbOpt,
    load_balancing::strategy::Adaptive,
    route::{
        AccessControl, CacheConfig, HeaderRules, HostRewrite, PassiveHealthConfig, RetryConfig,
        RouteAction, RouteConfig, TimeoutConfig, UpstreamTls,
    },
    server_builder::{Route, TlsConfig as BuilderTlsConfig},
};

/// Parse a CIDR network, or a bare IP address as a host network.
fn parse_net(value: &str) -> Result<IpNet> {
    if let Ok(net) = value.parse::<IpNet>() {
        return Ok(net);
    }
    let ip: IpAddr = value
        .parse()
        .wrap_err_with(|| format!("Invalid IP/CIDR '{value}'"))?;
    Ok(IpNet::from(ip))
}

fn parse_nets(values: &[String]) -> Result<Vec<IpNet>> {
    values.iter().map(|v| parse_net(v)).collect()
}

fn default_redirect_status() -> u16 {
    301
}

fn parse_header_pairs(map: &HashMap<String, String>) -> Result<Vec<(HeaderName, HeaderValue)>> {
    map.iter()
        .map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .wrap_err_with(|| format!("Invalid header name '{name}'"))?;
            let value = HeaderValue::from_str(value)
                .wrap_err_with(|| format!("Invalid header value '{value}'"))?;
            Ok((name, value))
        })
        .collect()
}

fn parse_header_names(names: &[String]) -> Result<Vec<HeaderName>> {
    names
        .iter()
        .map(|name| {
            HeaderName::from_bytes(name.as_bytes())
                .wrap_err_with(|| format!("Invalid header name '{name}'"))
        })
        .collect()
}

fn default_true() -> bool {
    true
}

fn default_weight() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub proxy: ProxyConfig,
}

impl Config {
    /// Build the routes described by the config.
    pub fn routes(&self) -> Result<Vec<Route>> {
        self.proxy
            .router
            .iter()
            .map(RouteEntry::to_route)
            .collect()
    }

    /// The plain (non-TLS) listen address, e.g. `0.0.0.0:3500`.
    pub fn listen_address(&self) -> String {
        format!("0.0.0.0:{}", self.proxy.listener)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerConfig {
    pub prometheus_address: Option<String>,
    pub set_strategy_endpoint: Option<String>,
    /// Per-request access log (nginx `access_log on/off`). Defaults to on when omitted.
    pub access_log: Option<bool>,
    /// Redirect plain-HTTP requests to `https://`. Defaults to off.
    pub https_redirect: Option<bool>,
    /// Downstream response compression level (nginx `gzip`); 0/omitted disables.
    pub compression_level: Option<u32>,
    /// Generate/propagate an `X-Request-Id` header for tracing. Defaults to off.
    pub request_id: Option<bool>,
    /// Tokio worker threads (nginx `worker_processes`). Omitted = Pingora default.
    pub worker_threads: Option<usize>,
    /// Upstream keepalive connection pool size. Omitted = 200000.
    pub upstream_keepalive_pool_size: Option<usize>,
    /// Graceful-shutdown grace period (seconds) for in-flight requests.
    pub grace_period_seconds: Option<u64>,
    /// Graceful-shutdown hard timeout (seconds).
    pub graceful_shutdown_timeout_seconds: Option<u64>,
}

impl ServerConfig {
    /// Whether any server-level runtime tuning was provided.
    pub fn has_runtime_tuning(&self) -> bool {
        self.worker_threads.is_some()
            || self.upstream_keepalive_pool_size.is_some()
            || self.grace_period_seconds.is_some()
            || self.graceful_shutdown_timeout_seconds.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    pub listener: u16,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub router: Vec<RouteEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub listener: u16,
    pub cert_path: String,
    pub key_path: String,
    #[serde(default = "default_true")]
    pub enable_h2: bool,
}

impl TlsConfig {
    pub fn to_builder_tls(&self) -> BuilderTlsConfig {
        BuilderTlsConfig {
            address: format!("0.0.0.0:{}", self.listener),
            cert_path: self.cert_path.clone(),
            key_path: self.key_path.clone(),
            enable_h2: self.enable_h2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteEntry {
    pub path: String,
    /// Optional virtual host (nginx `server_name`); routes with no host form the default server.
    pub host: Option<String>,
    /// Treat `path` as a regex location (default-server only) instead of a matchit prefix.
    #[serde(default)]
    pub regex: bool,
    /// Short-circuit response (redirect/return) instead of proxying.
    pub action: Option<ActionInput>,
    /// Response caching (nginx `proxy_cache`).
    pub cache: Option<CacheInput>,
    /// IP allow/deny + Basic auth (nginx `allow`/`deny`/`auth_basic`).
    pub access: Option<AccessInput>,
    #[serde(default = "default_true")]
    pub strip_prefix: bool,
    #[serde(default)]
    pub headers: HeadersConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub retry: RetryConfigInput,
    #[serde(default)]
    pub passive_health: PassiveHealthInput,
    /// nginx `client_max_body_size` in bytes; `None` = unlimited.
    pub max_body_size: Option<usize>,
    #[serde(default)]
    pub upstream_tls: UpstreamTlsInput,
    /// `Strict-Transport-Security` value to add on TLS responses.
    pub hsts: Option<String>,
    /// Max requests/sec per client IP (nginx `limit_req`).
    pub rate_limit_rps: Option<f64>,
    /// Max concurrent requests per client IP (nginx `limit_conn`).
    pub max_connections: Option<usize>,
    pub load_balancer: LoadBalancerConfig,
}

impl RouteEntry {
    fn to_route(&self) -> Result<Route> {
        let mut upstreams = self
            .load_balancer
            .upstreams
            .iter()
            .map(|u| (u.address.clone(), u.weight))
            .collect::<Vec<_>>();

        let config = self.route_config()?;

        // Redirect/return routes never proxy, but Route requires at least one backend; inject an
        // unused placeholder so such routes can be declared without an upstream.
        if upstreams.is_empty() && config.action.is_some() {
            upstreams.push(("127.0.0.1:1".to_string(), 1));
        }

        let lb_opt = self.load_balancer.to_lb_opt();
        let built = if self.regex {
            Route::regex(&self.path, upstreams, lb_opt)?
        } else {
            Route::with_weighted_backends(&self.path, upstreams, lb_opt)?
        };
        let mut route = built.route_config(config);
        if let Some(host) = &self.host {
            route = route.host(host.clone());
        }
        Ok(route)
    }

    /// Build just the per-route runtime config (no backends/LB). Used for hot reload.
    pub fn route_config(&self) -> Result<RouteConfig> {
        Ok(RouteConfig {
            strip_path_prefix: self.strip_prefix,
            headers: self.headers.to_rules()?,
            timeouts: self.timeouts.to_timeouts(),
            retry: self.retry.to_retry(),
            passive_health: self.passive_health.to_config(),
            max_body_size: self.max_body_size,
            upstream_tls: self.upstream_tls.to_upstream_tls(),
            hsts: self.hsts.clone(),
            rate_limit_rps: self.rate_limit_rps,
            max_connections: self.max_connections,
            action: self.action.as_ref().map(ActionInput::to_action),
            cache: self.cache.as_ref().map(CacheInput::to_cache),
            access: self.access.as_ref().map(AccessInput::to_access).transpose()?,
        })
    }

    /// Stable identity for matching a config entry to its live route at reload time:
    /// `(lowercased host, is_regex, transformed path)`.
    pub fn route_key(&self) -> (String, bool, String) {
        let host = self
            .host
            .as_deref()
            .map(|h| h.to_ascii_lowercase())
            .unwrap_or_default();
        let path = if self.regex {
            self.path.clone()
        } else {
            self.path
                .replacen('*', crate::utils::constants::DEFAULT_WILDCARD_IDENTIFIER, 1)
        };
        (host, self.regex, path)
    }
}

/// Access control config (nginx `allow`/`deny`/`auth_basic`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccessInput {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    pub basic_auth_realm: Option<String>,
    /// Accepted `user:password` credentials.
    #[serde(default)]
    pub basic_auth: Vec<String>,
}

impl AccessInput {
    fn to_access(&self) -> Result<AccessControl> {
        Ok(AccessControl {
            allow: parse_nets(&self.allow)?,
            deny: parse_nets(&self.deny)?,
            basic_auth_realm: self.basic_auth_realm.clone(),
            basic_auth: self
                .basic_auth
                .iter()
                .map(|cred| BASE64_STANDARD.encode(cred))
                .collect(),
        })
    }
}

/// Response caching config (nginx `proxy_cache`).
#[derive(Debug, Clone, Deserialize)]
pub struct CacheInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cache_ttl_secs")]
    pub ttl_secs: u64,
}

fn default_cache_ttl_secs() -> u64 {
    60
}

impl CacheInput {
    fn to_cache(&self) -> CacheConfig {
        CacheConfig {
            enabled: self.enabled,
            ttl: Duration::from_secs(self.ttl_secs),
        }
    }
}

/// Short-circuit response config (nginx `return` / `rewrite ... redirect`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionInput {
    Redirect {
        #[serde(default = "default_redirect_status")]
        status: u16,
        location: String,
    },
    Return {
        status: u16,
        #[serde(default)]
        body: Option<String>,
    },
}

impl ActionInput {
    fn to_action(&self) -> RouteAction {
        match self {
            ActionInput::Redirect { status, location } => RouteAction::Redirect {
                status: *status,
                location: location.clone(),
            },
            ActionInput::Return { status, body } => RouteAction::Return {
                status: *status,
                body: body.clone(),
            },
        }
    }
}

/// Header manipulation config for a route. Mirrors nginx `proxy_set_header` / `add_header`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeadersConfig {
    /// Add `X-Forwarded-For/Proto` and `X-Real-IP` (default true).
    pub forwarded: Option<bool>,
    /// Append to an existing `X-Forwarded-For` instead of resetting it (default false).
    pub trusted_proxy: Option<bool>,
    /// `"preserve"` (default) or a literal value to set the upstream `Host` header to.
    pub host: Option<String>,
    #[serde(default)]
    pub set_request: HashMap<String, String>,
    #[serde(default)]
    pub remove_request: Vec<String>,
    #[serde(default)]
    pub add_response: HashMap<String, String>,
    #[serde(default)]
    pub remove_response: Vec<String>,
}

/// Upstream timeouts in milliseconds. Unset fields keep Pingora's defaults.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TimeoutsConfig {
    pub connect_ms: Option<u64>,
    pub read_ms: Option<u64>,
    pub write_ms: Option<u64>,
    pub idle_ms: Option<u64>,
}

impl TimeoutsConfig {
    fn to_timeouts(&self) -> TimeoutConfig {
        TimeoutConfig {
            connect: self.connect_ms.map(Duration::from_millis),
            read: self.read_ms.map(Duration::from_millis),
            write: self.write_ms.map(Duration::from_millis),
            idle: self.idle_ms.map(Duration::from_millis),
        }
    }
}

/// Per-request failover config (nginx `proxy_next_upstream` for connect errors).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RetryConfigInput {
    pub max_retries: Option<usize>,
    pub retry_on_connect_error: Option<bool>,
}

impl RetryConfigInput {
    fn to_retry(&self) -> RetryConfig {
        let mut retry = RetryConfig::default();
        if let Some(max) = self.max_retries {
            retry.max_retries = max;
        }
        if let Some(on_connect) = self.retry_on_connect_error {
            retry.retry_on_connect_error = on_connect;
        }
        retry
    }
}

/// Upstream TLS config (nginx `proxy_pass https://`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpstreamTlsInput {
    #[serde(default)]
    pub enabled: bool,
    pub sni: Option<String>,
    pub verify: Option<bool>,
    /// Negotiate HTTP/2 to the upstream (required for gRPC).
    #[serde(default)]
    pub h2: bool,
}

impl UpstreamTlsInput {
    fn to_upstream_tls(&self) -> UpstreamTls {
        UpstreamTls {
            enabled: self.enabled,
            sni: self.sni.clone(),
            verify: self.verify.unwrap_or(true),
            h2: self.h2,
        }
    }
}

/// Passive health checking config (nginx `max_fails` / `fail_timeout`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PassiveHealthInput {
    pub enabled: Option<bool>,
    pub max_fails: Option<u32>,
    pub fail_timeout_secs: Option<u64>,
}

impl PassiveHealthInput {
    fn to_config(&self) -> PassiveHealthConfig {
        let mut config = PassiveHealthConfig::default();
        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
        if let Some(max_fails) = self.max_fails {
            config.max_fails = max_fails;
        }
        if let Some(secs) = self.fail_timeout_secs {
            config.fail_timeout = Duration::from_secs(secs);
        }
        config
    }
}

impl HeadersConfig {
    fn to_rules(&self) -> Result<HeaderRules> {
        let mut rules = HeaderRules::default();
        if let Some(forwarded) = self.forwarded {
            rules.forwarded = forwarded;
        }
        if let Some(trusted) = self.trusted_proxy {
            rules.trusted_proxy = trusted;
        }
        if let Some(host) = &self.host {
            rules.host = if host.eq_ignore_ascii_case("preserve") {
                HostRewrite::Preserve
            } else {
                HostRewrite::Set(host.clone())
            };
        }
        rules.set_request = parse_header_pairs(&self.set_request)?;
        rules.remove_request = parse_header_names(&self.remove_request)?;
        rules.add_response = parse_header_pairs(&self.add_response)?;
        rules.remove_response = parse_header_names(&self.remove_response)?;
        Ok(rules)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadBalancerConfig {
    /// Starting strategy for the adaptive load balancer.
    #[serde(default)]
    pub strategy: Adaptive,
    #[serde(default)]
    pub adaptive_lb_opt: AdaptiveLbOptConfig,
    pub max_iterations: Option<usize>,
    /// Health check interval in seconds; `0` disables the health check background service.
    pub health_check_interval_secs: Option<u64>,
    pub upstreams: Vec<UpstreamConfig>,
}

impl LoadBalancerConfig {
    /// Merge the config over [`AdaptiveLbOpt::default`], leaving unset fields at their defaults.
    fn to_lb_opt(&self) -> AdaptiveLbOpt {
        let mut opt = AdaptiveLbOpt {
            starting_strategy: self.strategy.clone(),
            ..Default::default()
        };

        if let Some(max_iterations) = self.max_iterations {
            opt.max_iterations = max_iterations;
        }
        if let Some(secs) = self.health_check_interval_secs {
            opt.health_check_interval = (secs > 0).then(|| Duration::from_secs(secs));
        }

        let a = &self.adaptive_lb_opt;
        if let Some(v) = a.latency_smoothing_factor {
            opt.latency_smoothing_factor = v;
        }
        if let Some(v) = a.connections_divergence_ratio {
            opt.connections_divergence_ratio = v;
        }
        if let Some(v) = a.latency_divergence_ratio {
            opt.latency_divergence_ratio = v;
        }
        if let Some(secs) = a.evaluate_strategy_frequency_secs {
            opt.evaluate_strategy_frequency = Duration::from_secs(secs);
        }
        if let Some(v) = a.min_nr_of_connections {
            opt.min_nr_of_connections = v;
        }
        if let Some(v) = a.hysteresis_exit_factor {
            opt.hysteresis_exit_factor = v;
        }

        opt
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AdaptiveLbOptConfig {
    pub latency_smoothing_factor: Option<f32>,
    pub connections_divergence_ratio: Option<f32>,
    pub latency_divergence_ratio: Option<f32>,
    pub evaluate_strategy_frequency_secs: Option<u64>,
    pub min_nr_of_connections: Option<usize>,
    pub hysteresis_exit_factor: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    pub address: String,
    /// Relative weight; proportionally biases load-balancing selection (nginx `weight=N`).
    #[serde(default = "default_weight")]
    pub weight: usize,
}
