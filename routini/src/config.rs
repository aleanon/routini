//! Declarative configuration for the proxy, read from a JSON or TOML file (see
//! [crate::utils::config_loader] for format detection and lookup).
//!
//! The shapes here mirror `config.json` and convert into the builder types in
//! [`crate::server_builder`], so `main` can construct the whole server from a file instead of
//! hard-coded values.
use std::{collections::HashMap, net::IpAddr, time::Duration};

use base64::prelude::{BASE64_STANDARD, Engine};
use std::net::{SocketAddr, TcpListener};

use pingora::server::configuration::ServerConf;

use color_eyre::eyre::{Result, WrapErr, eyre};
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
    server_builder::{Route, ServerBuilder, TlsConfig as BuilderTlsConfig, proxy_server},
    utils::{
        config_loader::ConfigFormat,
        constants::{
            LISTEN_ADDRESS_ENV_VAR, PROMETHEUS_ADDRESS_ENV_VAR, PROMETHEUS_ENDPOINT_ADDRESS,
        },
    },
};

/// Two addresses collide if they share a port and either the IPs match or one is a wildcard.
fn overlaps(a: &SocketAddr, b: &SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub proxy: ProxyConfig,
}

impl Config {
    /// Parse a config in `format` without validating it.
    pub fn parse(contents: &str, format: ConfigFormat) -> Result<Self> {
        match format {
            ConfigFormat::Json => serde_json::from_str(contents).map_err(|err| eyre!("{err}")),
            ConfigFormat::Toml => toml::from_str(contents).map_err(|err| eyre!("{err}")),
        }
    }

    /// Override server level settings from the environment, `get_env` is injectable for tests.
    /// `SERVER_ADDRESS` replaces `proxy.listener`, `PROMETHEUS_ADDRESS` the metrics address.
    pub fn apply_env_overrides(&mut self, get_env: impl Fn(&str) -> Option<String>) {
        let get = |key| get_env(key).filter(|v: &String| !v.trim().is_empty());
        if let Some(address) = get(LISTEN_ADDRESS_ENV_VAR) {
            self.proxy.listener = Listen::Address(address);
        }
        if let Some(address) = get(PROMETHEUS_ADDRESS_ENV_VAR) {
            self.server.prometheus_address = Some(address);
        }
    }

    /// Check the whole config and report every problem found, not just the first.
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        // Listen addresses must parse and not collide
        let mut addresses: Vec<(&str, SocketAddr)> = Vec::new();
        let listeners = [
            ("proxy.listener", Some(self.proxy.listener.address())),
            (
                "proxy.tls.listener",
                self.proxy.tls.as_ref().map(|t| t.listener.address()),
            ),
            (
                "server.prometheus_address",
                Some(
                    self.server
                        .prometheus_address
                        .clone()
                        .unwrap_or_else(|| PROMETHEUS_ENDPOINT_ADDRESS.to_string()),
                ),
            ),
        ];
        for (field, address) in &listeners {
            let Some(address) = address else { continue };
            match address.parse::<SocketAddr>() {
                Ok(addr) => {
                    if let Some((other, _)) = addresses.iter().find(|(_, o)| overlaps(o, &addr)) {
                        errors.push(format!("{field} ({address}) conflicts with {other}"));
                    }
                    addresses.push((field, addr));
                }
                Err(err) => errors.push(format!(
                    "{field}: '{address}' is not a valid socket address ({err})"
                )),
            }
        }

        if let Some(level) = self.server.compression_level {
            if level > 9 {
                errors.push(format!("server.compression_level must be 0-9, got {level}"));
            }
        }
        for code in self.server.error_pages.keys() {
            if !(100..=599).contains(code) {
                errors.push(format!(
                    "server.error_pages: {code} is not an HTTP status code"
                ));
            }
        }
        if self.server.worker_threads == Some(0) {
            errors.push("server.worker_threads must be greater than 0".to_string());
        }

        if self.proxy.router.is_empty() {
            errors.push("proxy.router must define at least one route".to_string());
        }

        // Insert into scratch routers per virtual host so conflicting paths are reported here
        // instead of panicking when the server is built.
        let mut routers: HashMap<String, matchit::Router<()>> = HashMap::new();
        for entry in &self.proxy.router {
            let name = entry.display_name();
            entry.validate(&name, &mut errors);
            if entry.regex {
                continue;
            }
            let (host, _, path) = entry.route_key();
            if let Err(err) = routers.entry(host).or_default().insert(path, ()) {
                errors.push(format!("{name}: {err}"));
            }
        }

        if errors.is_empty() {
            return Ok(());
        }
        Err(eyre!(
            "{} problem(s) found:\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        ))
    }

    /// Turn the config into a [ServerBuilder] with every server and route setting applied. Call
    /// [Self::validate] first ([crate::utils::config_loader::load_config_from] does). The config is
    /// kept as the builder's applied config, so reloads can diff against it.
    pub fn into_server_builder(self) -> Result<ServerBuilder> {
        // Binding up front gives a clear error if the port is taken. The listener is dropped
        // again, pingora binds the address itself when the service starts.
        let listener = TcpListener::bind(self.listen_address())
            .wrap_err_with(|| format!("Failed to bind to {}", self.listen_address()))?;
        let mut builder = proxy_server(listener);

        let server = &self.server;
        if server.has_runtime_tuning() {
            let mut conf = ServerConf::default();
            conf.upstream_keepalive_pool_size =
                server.upstream_keepalive_pool_size.unwrap_or(200000);
            if let Some(threads) = server.worker_threads {
                conf.threads = threads;
            }
            conf.grace_period_seconds = server.grace_period_seconds;
            conf.graceful_shutdown_timeout_seconds = server.graceful_shutdown_timeout_seconds;
            builder = builder.server_config(conf);
        }
        for route in self.routes()? {
            builder = builder.add_route(route);
        }
        if let Some(address) = server.prometheus_address.clone() {
            builder = builder.prometheus_address(address);
        }
        if let Some(access_log) = server.access_log {
            builder = builder.access_log(access_log);
        }
        if let Some(https_redirect) = server.https_redirect {
            builder = builder.https_redirect(https_redirect);
        }
        if let Some(level) = server.compression_level {
            builder = builder.compression_level(level);
        }
        if let Some(request_id) = server.request_id {
            builder = builder.request_id(request_id);
        }
        if !server.error_pages.is_empty() {
            builder = builder.error_pages(server.error_pages.clone());
        }
        if let Some(tls) = &self.proxy.tls {
            builder = builder.tls(tls.to_builder_tls());
        }
        Ok(builder.applied_config(self))
    }

    /// Build the routes described by the config.
    pub fn routes(&self) -> Result<Vec<Route>> {
        self.proxy.router.iter().map(RouteEntry::to_route).collect()
    }

    /// The plain (non-TLS) listen address, e.g. `0.0.0.0:3500`.
    pub fn listen_address(&self) -> String {
        self.proxy.listener.address()
    }
}

/// A listen address: a port (`3500`, bound on all interfaces) or a full `"host:port"`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Listen {
    Port(u16),
    Address(String),
}

impl Listen {
    pub fn address(&self) -> String {
        match self {
            Listen::Port(port) => format!("0.0.0.0:{port}"),
            Listen::Address(address) => address.clone(),
        }
    }
}

impl From<u16> for Listen {
    fn from(port: u16) -> Self {
        Listen::Port(port)
    }
}

/// Status-code keyed map. TOML (unlike JSON via serde_json) does not turn string keys into
/// integers, so keys are read as strings and parsed for both formats.
fn deserialize_status_map<'de, D>(
    deserializer: D,
) -> std::result::Result<HashMap<u16, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = HashMap::<String, String>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(code, body)| {
            code.trim()
                .parse::<u16>()
                .map(|code| (code, body))
                .map_err(|_| serde::de::Error::custom(format!("invalid status code '{code}'")))
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub prometheus_address: Option<String>,
    /// Per-request access log (nginx `access_log on/off`). Defaults to on when omitted.
    pub access_log: Option<bool>,
    /// Redirect plain-HTTP requests to `https://`. Defaults to off.
    pub https_redirect: Option<bool>,
    /// Downstream response compression level (nginx `gzip`); 0/omitted disables.
    pub compression_level: Option<u32>,
    /// Generate/propagate an `X-Request-Id` header for tracing. Defaults to off.
    pub request_id: Option<bool>,
    /// Custom error-page bodies keyed by status code (nginx `error_page`).
    #[serde(default, deserialize_with = "deserialize_status_map")]
    pub error_pages: HashMap<u16, String>,
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub listener: Listen,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub router: Vec<RouteEntry>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub listener: Listen,
    pub cert_path: String,
    pub key_path: String,
    #[serde(default = "default_true")]
    pub enable_h2: bool,
}

impl TlsConfig {
    pub fn to_builder_tls(&self) -> BuilderTlsConfig {
        BuilderTlsConfig {
            address: self.listener.address(),
            cert_path: self.cert_path.clone(),
            key_path: self.key_path.clone(),
            enable_h2: self.enable_h2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// `route '/api/*'` or `route 'example.com/api/*'`, for error messages.
    fn display_name(&self) -> String {
        match &self.host {
            Some(host) => format!("route '{host}{}'", self.path),
            None => format!("route '{}'", self.path),
        }
    }

    fn validate(&self, name: &str, errors: &mut Vec<String>) {
        if let Err(err) = self.to_route() {
            errors.push(format!("{name}: {err:#}"));
        }
        if self.load_balancer.upstreams.is_empty() && self.action.is_none() {
            errors.push(format!(
                "{name}: load_balancer.upstreams is empty (only redirect/return routes may omit it)"
            ));
        }
        if let Some(action) = &self.action {
            let (field, status, allowed) = match action {
                ActionInput::Redirect { status, .. } => {
                    ("action.redirect.status", *status, 300..=399)
                }
                ActionInput::Return { status, .. } => ("action.return.status", *status, 100..=599),
            };
            if !allowed.contains(&status) {
                errors.push(format!(
                    "{name}: {field} {status} is out of range {allowed:?}"
                ));
            }
        }
        if self.rate_limit_rps.is_some_and(|rps| !(rps > 0.0)) {
            errors.push(format!("{name}: rate_limit_rps must be greater than 0"));
        }
        if self.max_connections == Some(0) {
            errors.push(format!("{name}: max_connections must be greater than 0"));
        }

        let lb = &self.load_balancer;
        if lb.max_iterations == Some(0) {
            errors.push(format!(
                "{name}: load_balancer.max_iterations must be greater than 0"
            ));
        }
        for upstream in &lb.upstreams {
            if upstream.weight == 0 {
                errors.push(format!(
                    "{name}: upstream {} has weight 0, weights must be at least 1",
                    upstream.address
                ));
            }
        }
        let a = &lb.adaptive_lb_opt;
        // Divergence ratios are max / min and never below 1, a lower threshold always triggers
        for (field, value) in [
            (
                "connections_divergence_ratio",
                a.connections_divergence_ratio,
            ),
            ("latency_divergence_ratio", a.latency_divergence_ratio),
        ] {
            if value.is_some_and(|v| !(v >= 1.0)) {
                errors.push(format!(
                    "{name}: adaptive_lb_opt.{field} must be >= 1.0, got {}",
                    value.unwrap()
                ));
            }
        }
        if a.latency_smoothing_factor
            .is_some_and(|v| !(v > 0.0 && v <= 1.0))
        {
            errors.push(format!(
                "{name}: adaptive_lb_opt.latency_smoothing_factor must be in (0, 1]"
            ));
        }
        if a.hysteresis_exit_factor
            .is_some_and(|v| !(0.0..=1.0).contains(&v))
        {
            errors.push(format!(
                "{name}: adaptive_lb_opt.hysteresis_exit_factor must be in [0, 1]"
            ));
        }
        if a.evaluate_strategy_frequency_secs == Some(0) {
            errors.push(format!(
                "{name}: adaptive_lb_opt.evaluate_strategy_frequency_secs must be greater than 0"
            ));
        }
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
            access: self
                .access
                .as_ref()
                .map(AccessInput::to_access)
                .transpose()?,
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptiveLbOptConfig {
    pub latency_smoothing_factor: Option<f32>,
    pub connections_divergence_ratio: Option<f32>,
    pub latency_divergence_ratio: Option<f32>,
    pub evaluate_strategy_frequency_secs: Option<u64>,
    pub min_nr_of_connections: Option<usize>,
    pub hysteresis_exit_factor: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub address: String,
    /// Relative weight; proportionally biases load-balancing selection (nginx `weight=N`).
    #[serde(default = "default_weight")]
    pub weight: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_JSON: &str = r#"{"proxy": {"listener": 3500, "router": [
        {"path": "/api/*", "load_balancer": {"upstreams": [{"address": "127.0.0.1:4001"}]}}
    ]}}"#;

    const MINIMAL_TOML: &str = r#"
        [proxy]
        listener = 3500
        [[proxy.router]]
        path = "/api/*"
        [[proxy.router.load_balancer.upstreams]]
        address = "127.0.0.1:4001"
    "#;

    fn toml(s: &str) -> Config {
        Config::parse(s, ConfigFormat::Toml).unwrap()
    }

    fn errors_of(config: &Config) -> String {
        config.validate().unwrap_err().to_string()
    }

    #[test]
    fn json_and_toml_parse_to_the_same_config() {
        let json = Config::parse(MINIMAL_JSON, ConfigFormat::Json).unwrap();
        assert_eq!(json, toml(MINIMAL_TOML));
        json.validate().unwrap();
    }

    #[test]
    fn shipped_json_and_example_toml_are_equivalent_and_valid() {
        let json = Config::parse(include_str!("../config.json"), ConfigFormat::Json).unwrap();
        let toml = toml(include_str!("../config.example.toml"));
        assert_eq!(json, toml);
        json.validate().unwrap();
    }

    #[test]
    fn unknown_keys_are_rejected_in_both_formats() {
        let err = Config::parse(
            &MINIMAL_JSON.replace("\"path\"", "\"strip_prefx\": true, \"path\""),
            ConfigFormat::Json,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("strip_prefx"), "{err}");

        let err = Config::parse(
            &format!("{MINIMAL_TOML}\n[server]\nprometheus_adress = \"0.0.0.0:9000\""),
            ConfigFormat::Toml,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("prometheus_adress"), "{err}");
    }

    #[test]
    fn error_pages_with_status_keys_parse_from_toml() {
        let config = toml(&format!(
            "[server.error_pages]\n404 = \"<h1>nope</h1>\"\n\"503\" = \"down\"\n{MINIMAL_TOML}"
        ));
        assert_eq!(config.server.error_pages[&404], "<h1>nope</h1>");
        assert_eq!(config.server.error_pages[&503], "down");

        let err = Config::parse(
            &format!("[server.error_pages]\nabc = \"x\"\n{MINIMAL_TOML}"),
            ConfigFormat::Toml,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid status code"), "{err}");
    }

    #[test]
    fn listener_accepts_a_port_or_an_address() {
        assert_eq!(toml(MINIMAL_TOML).listen_address(), "0.0.0.0:3500");
        let config =
            toml(&MINIMAL_TOML.replace("listener = 3500", "listener = \"127.0.0.1:3600\""));
        assert_eq!(config.listen_address(), "127.0.0.1:3600");
    }

    #[test]
    fn env_overrides_listener_and_prometheus() {
        let mut config = toml(MINIMAL_TOML);
        config.apply_env_overrides(|key| match key {
            LISTEN_ADDRESS_ENV_VAR => Some("127.0.0.1:8000".into()),
            PROMETHEUS_ADDRESS_ENV_VAR => Some("127.0.0.1:9191".into()),
            _ => None,
        });
        assert_eq!(config.listen_address(), "127.0.0.1:8000");
        assert_eq!(
            config.server.prometheus_address.as_deref(),
            Some("127.0.0.1:9191")
        );
    }

    #[test]
    fn all_problems_are_reported_together() {
        let config = toml(
            r#"
            [server]
            compression_level = 12
            prometheus_address = "0.0.0.0:3500"
            [server.error_pages]
            999 = "x"

            [proxy]
            listener = 3500

            [[proxy.router]]
            path = "api"
            [[proxy.router.load_balancer.upstreams]]
            address = "127.0.0.1"

            [[proxy.router]]
            path = "/empty/*"
            rate_limit_rps = 0
            [proxy.router.load_balancer]
            max_iterations = 0
            upstreams = []
            [proxy.router.load_balancer.adaptive_lb_opt]
            connections_divergence_ratio = 0.9
            hysteresis_exit_factor = 2.0
            latency_smoothing_factor = 0.0

            [[proxy.router]]
            path = "/r"
            action = { redirect = { status = 200, location = "/x" } }
            [proxy.router.load_balancer]
            upstreams = []
        "#,
        );
        let err = errors_of(&config);
        for expected in [
            "compression_level",
            "conflicts with proxy.listener",
            "999 is not an HTTP status code",
            "Invalid path",
            "load_balancer.upstreams is empty",
            "rate_limit_rps",
            "max_iterations",
            "connections_divergence_ratio",
            "hysteresis_exit_factor",
            "latency_smoothing_factor",
            "action.redirect.status 200",
        ] {
            assert!(err.contains(expected), "missing '{expected}' in:\n{err}");
        }
    }

    #[test]
    fn invalid_backend_address_is_an_error_not_a_panic() {
        let config = toml(&MINIMAL_TOML.replace("127.0.0.1:4001", "not-an-address"));
        assert!(errors_of(&config).contains("not-an-address"));
    }

    #[test]
    fn conflicting_routes_are_reported_per_virtual_host() {
        let dup = format!(
            "{MINIMAL_TOML}\n[[proxy.router]]\npath = \"/api/*\"\n\
             [[proxy.router.load_balancer.upstreams]]\naddress = \"127.0.0.1:4002\"\n"
        );
        assert!(errors_of(&toml(&dup)).contains("route '/api/*'"));

        // The same path on a different virtual host is fine
        let vhost = dup.replacen(
            "[[proxy.router]]\npath = \"/api/*\"\n[[",
            "[[proxy.router]]\npath = \"/api/*\"\nhost = \"example.com\"\n[[",
            1,
        );
        toml(&vhost).validate().unwrap();
    }

    #[test]
    fn invalid_regex_route_is_reported() {
        let config = toml(
            &MINIMAL_TOML.replace("path = \"/api/*\"", "path = \"^/(unclosed\"\nregex = true"),
        );
        assert!(errors_of(&config).contains("Invalid regex"));
    }
}
