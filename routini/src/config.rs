//! Declarative configuration for the proxy, read from a JSON or TOML file (see
//! [crate::utils::config_loader] for format detection and lookup).
//!
//! The shapes here mirror `config.json` and convert into the builder types in
//! [`crate::server_builder`], so `main` can construct the whole server from a file instead of
//! hard-coded values.
use std::{collections::HashMap, net::IpAddr, time::Duration};

use std::net::{SocketAddr, TcpListener};

use pingora::server::configuration::ServerConf;

use color_eyre::eyre::{Result, WrapErr, eyre};
use http::{HeaderName, HeaderValue};
use ipnet::IpNet;
use serde::Deserialize;

use crate::{
    adaptive_loadbalancer::options::{
        AdaptiveLbOpt, HealthCheckOpt, HealthProbe, HttpProbe, ProbeTls,
    },
    basic_auth::BasicAuth,
    load_balancing::strategy::Adaptive,
    proxy::ClientTimeouts,
    rate_limit::RateLimit,
    real_ip::RealIp,
    route::{
        AccessControl, CacheConfig, HeaderRules, HostRewrite, PassiveHealthConfig, RetryConfig,
        RouteAction, RouteConfig, TimeoutConfig, UpstreamTls,
    },
    server_builder::{Route, ServerBuilder, TlsConfig as BuilderTlsConfig},
    tls::{self, CertPair, TlsOptions, TlsVersion},
    utils::{
        config_loader::ConfigFormat,
        constants::{
            LISTEN_ADDRESS_ENV_VAR, PID_FILE_NAME, PROMETHEUS_ADDRESS_ENV_VAR,
            PROMETHEUS_ENDPOINT_ADDRESS, UPGRADE_SOCK_NAME,
        },
        runtime_paths::default_runtime_dir,
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

    /// Make relative TLS file paths relative to `config_dir` (the config file's directory), so
    /// they don't depend on the working directory routini is started from.
    pub fn resolve_paths(&mut self, config_dir: &std::path::Path) {
        if let Some(tls) = &mut self.proxy.tls {
            let paths = [&mut tls.cert_path, &mut tls.key_path]
                .into_iter()
                .chain(tls.client_ca_path.as_mut())
                .chain(
                    tls.certificates
                        .iter_mut()
                        .flat_map(|c| [&mut c.cert_path, &mut c.key_path]),
                );
            for path in paths {
                if std::path::Path::new(path.as_str()).is_relative() {
                    *path = config_dir.join(&*path).to_string_lossy().into_owned();
                }
            }
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

    /// Every address routini listens on, by config field.
    fn listen_addresses(&self) -> Vec<(&'static str, String)> {
        let mut addresses = vec![("proxy.listener", self.proxy.listener.address())];
        if let Some(tls) = &self.proxy.tls {
            addresses.push(("proxy.tls.listener", tls.listener.address()));
        }
        addresses.push((
            "server.prometheus_address",
            self.server
                .prometheus_address
                .clone()
                .unwrap_or_else(|| PROMETHEUS_ENDPOINT_ADDRESS.to_string()),
        ));
        addresses
    }

    /// Check the whole config and report every problem found, not just the first.
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        // Listen addresses must parse and not collide
        let mut addresses: Vec<(&str, SocketAddr)> = Vec::new();
        for (field, address) in &self.listen_addresses() {
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
        if let Some(tls) = &self.proxy.tls {
            match tls.to_options() {
                Ok(options) => {
                    if let Err(err) = tls::check(&options) {
                        errors.push(format!("proxy.tls: {err:#}"));
                    }
                }
                Err(err) => errors.push(format!("proxy.tls: {err}")),
            }
        }

        let upgrade_sock = self.server.upgrade_sock();
        if upgrade_sock.len() > MAX_UNIX_SOCKET_PATH {
            errors.push(format!(
                "server.upgrade_sock is {} bytes, unix socket paths can be at most \
                 {MAX_UNIX_SOCKET_PATH}",
                upgrade_sock.len()
            ));
        }
        if let Some(real_ip) = &self.server.real_ip {
            if real_ip.trusted_proxies.is_empty() {
                errors.push(
                    "server.real_ip.trusted_proxies is empty, no proxy would be believed"
                        .to_string(),
                );
            }
            if let Err(err) = real_ip.to_real_ip() {
                errors.push(format!("server.real_ip: {err}"));
            }
        }
        for (field, value) in [
            (
                "client_body_timeout_secs",
                self.server.client_body_timeout_secs,
            ),
            ("send_timeout_secs", self.server.send_timeout_secs),
        ] {
            if value == Some(0) {
                errors.push(format!("server.{field} must be greater than 0"));
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
        self.into_server_builder_with(false)
    }

    /// [Self::into_server_builder], with `upgrade` taking over the listening sockets of a running
    /// routini instead of binding them (graceful upgrade).
    pub fn into_server_builder_with(self, upgrade: bool) -> Result<ServerBuilder> {
        if !upgrade {
            // Binding up front gives a clear error if a port is taken. Otherwise pingora retries
            // the bind forever while holding its listener lock, which blocks the other listeners
            // and graceful upgrades. The listeners are dropped again, pingora binds the
            // addresses itself. Skipped on upgrade: the old process still holds the ports and
            // hands them over.
            for (field, address) in self.listen_addresses() {
                TcpListener::bind(&address)
                    .wrap_err_with(|| format!("Failed to bind {field} {address}"))?;
            }
        }
        let mut builder = ServerBuilder::new(self.listen_address())
            .pid_file(self.server.pid_file())
            .upgrade_sock(self.server.upgrade_sock())
            .upgrade(upgrade);

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
        if let Some(real_ip) = &server.real_ip {
            builder = builder.real_ip(real_ip.to_real_ip()?);
        }
        builder = builder.client_timeouts(server.client_timeouts());
        if let Some(tls) = &self.proxy.tls {
            builder = builder.tls(tls.to_builder_tls()?);
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
    /// PID file written with `--daemon`. Default: `$XDG_RUNTIME_DIR/routini.pid`, or
    /// `/run/routini/routini.pid`. Refused in directories other users can write to.
    pub pid_file: Option<String>,
    /// Unix socket the listening sockets are handed over on during `--upgrade`; the old and new
    /// process must use the same path. Default and restrictions as for `pid_file`.
    pub upgrade_sock: Option<String>,
    /// Take the client address from a header set by trusted proxies (nginx `real_ip`).
    pub real_ip: Option<RealIpInput>,
    /// Idle seconds before a kept-alive client connection is closed, 0 disables keepalive
    /// (nginx `keepalive_timeout`). Default 75.
    pub keepalive_timeout_secs: Option<u64>,
    /// Longest wait for more of a request body (nginx `client_body_timeout`). Default 60.
    pub client_body_timeout_secs: Option<u64>,
    /// Longest wait for a client to take more of the response (nginx `send_timeout`).
    /// Default 60.
    pub send_timeout_secs: Option<u64>,
}

impl ServerConfig {
    pub fn client_timeouts(&self) -> ClientTimeouts {
        let mut t = ClientTimeouts::default();
        if let Some(secs) = self.keepalive_timeout_secs {
            t.keepalive_secs = secs;
        }
        if let Some(secs) = self.client_body_timeout_secs {
            t.body = Duration::from_secs(secs);
        }
        if let Some(secs) = self.send_timeout_secs {
            t.send = Duration::from_secs(secs);
        }
        t
    }
}

/// nginx `set_real_ip_from` / `real_ip_header` / `real_ip_recursive on`, see [RealIp].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealIpInput {
    /// Addresses or CIDR networks of the proxies in front of routini.
    pub trusted_proxies: Vec<String>,
    /// Header the proxies put the client address in. Default `X-Forwarded-For`.
    #[serde(default = "default_real_ip_header")]
    pub header: String,
}

fn default_real_ip_header() -> String {
    "X-Forwarded-For".to_string()
}

impl RealIpInput {
    pub fn to_real_ip(&self) -> Result<RealIp> {
        Ok(RealIp {
            trusted: parse_nets(&self.trusted_proxies)?,
            header: HeaderName::from_bytes(self.header.as_bytes())
                .wrap_err_with(|| format!("Invalid header name '{}'", self.header))?,
        })
    }
}

impl ServerConfig {
    pub fn pid_file(&self) -> String {
        self.pid_file
            .clone()
            .unwrap_or_else(|| default_runtime_path(PID_FILE_NAME))
    }

    pub fn upgrade_sock(&self) -> String {
        self.upgrade_sock
            .clone()
            .unwrap_or_else(|| default_runtime_path(UPGRADE_SOCK_NAME))
    }
}

fn default_runtime_path(name: &str) -> String {
    default_runtime_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// sun_path holds 108 bytes including the terminating NUL; pingora panics on longer paths.
const MAX_UNIX_SOCKET_PATH: usize = 107;

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
    /// The default certificate: served without SNI, or when no other certificate matches.
    pub cert_path: String,
    pub key_path: String,
    #[serde(default = "default_true")]
    pub enable_h2: bool,
    /// More certificates, each served for the DNS names it lists (nginx: one `server` block
    /// with `ssl_certificate` per name). Reloaded with the config, no restart needed.
    #[serde(default)]
    pub certificates: Vec<CertInput>,
    /// Lowest protocol version accepted: "1.2" (default) or "1.3" (nginx `ssl_protocols`).
    pub min_version: Option<String>,
    /// Require client certificates issued by these CAs (nginx `ssl_verify_client on`).
    pub client_ca_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertInput {
    pub cert_path: String,
    pub key_path: String,
}

impl TlsConfig {
    pub fn to_options(&self) -> Result<TlsOptions> {
        let min_version = match self.min_version.as_deref() {
            None | Some("1.2") => TlsVersion::Tls12,
            Some("1.3") => TlsVersion::Tls13,
            Some(other) => {
                return Err(eyre!(
                    "min_version must be \"1.2\" or \"1.3\", got \"{other}\""
                ));
            }
        };
        Ok(TlsOptions {
            default_cert: CertPair {
                cert_path: self.cert_path.clone(),
                key_path: self.key_path.clone(),
            },
            certificates: self
                .certificates
                .iter()
                .map(|c| CertPair {
                    cert_path: c.cert_path.clone(),
                    key_path: c.key_path.clone(),
                })
                .collect(),
            min_version,
            client_ca_path: self.client_ca_path.clone(),
            enable_h2: self.enable_h2,
        })
    }

    pub fn to_builder_tls(&self) -> Result<BuilderTlsConfig> {
        Ok(BuilderTlsConfig {
            address: self.listener.address(),
            options: self.to_options()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub path: String,
    /// Optional virtual host (nginx `server_name`): a name, `*.example.com` (any subdomain, the
    /// longest wildcard wins, an exact name before any) or an IP. Routes with no host form the
    /// default server.
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
    /// Sustained requests per second per client IP (nginx `limit_req rate=`).
    pub rate_limit_rps: Option<f64>,
    /// Requests a client may send above the rate at once (nginx `burst`). Default: one
    /// second's worth, `rate_limit_rps` rounded down.
    pub rate_limit_burst: Option<u32>,
    /// Hold requests within the burst until their slot, so they reach the backend at the rate
    /// (nginx without `nodelay`). Default false: they pass at once (`nodelay`).
    pub rate_limit_delay: Option<bool>,
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

        let lb_opt = self.load_balancer.to_lb_opt(&self.upstream_tls)?;
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
        if let Some(host) = &self.host {
            let bare = host.strip_prefix("*.").unwrap_or(host);
            let valid = !bare.is_empty()
                && !bare.contains('*')
                && bare.split('.').all(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                });
            if !valid && host.parse::<IpAddr>().is_err() {
                errors.push(format!(
                    "{name}: host '{host}' must be a name like example.com, a wildcard like \
                     *.example.com, or an IP address"
                ));
            }
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
        if self
            .rate_limit_rps
            .is_some_and(|rps| !(rps >= 0.001) || rps.is_infinite())
        {
            errors.push(format!(
                "{name}: rate_limit_rps must be at least 0.001 (one request per 1000s)"
            ));
        }
        if self.rate_limit_rps.is_none()
            && (self.rate_limit_burst.is_some() || self.rate_limit_delay.is_some())
        {
            errors.push(format!(
                "{name}: rate_limit_burst and rate_limit_delay need rate_limit_rps"
            ));
        }
        if let Some(code) = self
            .retry
            .retry_on_status
            .iter()
            .find(|c| !(400..=599).contains(*c))
        {
            errors.push(format!(
                "{name}: retry.retry_on_status {code} is not an error status (400-599)"
            ));
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
            rate_limit: self.rate_limit_rps.map(|rps| RateLimit {
                rps,
                burst: self.rate_limit_burst.unwrap_or(rps as u32),
                delay: self.rate_limit_delay.unwrap_or(false),
            }),
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
    /// Accepted users: `user:<bcrypt hash>` as `htpasswd -nB user` prints it, or
    /// `user:password`.
    #[serde(default)]
    pub basic_auth: Vec<String>,
}

impl AccessInput {
    fn to_access(&self) -> Result<AccessControl> {
        Ok(AccessControl {
            allow: parse_nets(&self.allow)?,
            deny: parse_nets(&self.deny)?,
            basic_auth_realm: self.basic_auth_realm.clone(),
            basic_auth: BasicAuth::new(&self.basic_auth).map_err(|err| eyre!("access: {err}"))?,
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
    /// Retry when the backend times out (default true; idempotent methods only).
    pub retry_on_timeout: Option<bool>,
    /// Retry when the backend answers with one of these statuses, e.g. [502, 503, 504].
    #[serde(default)]
    pub retry_on_status: Vec<u16>,
    /// Also retry POST, PATCH and LOCK once they reached a backend (default false).
    pub retry_non_idempotent: Option<bool>,
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
        if let Some(on_timeout) = self.retry_on_timeout {
            retry.retry_on_timeout = on_timeout;
        }
        retry.retry_on_status = self.retry_on_status.clone();
        if let Some(non_idempotent) = self.retry_non_idempotent {
            retry.retry_non_idempotent = non_idempotent;
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
    /// What FNVHash/Consistent hash: `client_ip` (default), `uri`, `path`, `header:<name>` or
    /// `cookie:<name>`.
    pub hash_key: Option<String>,
    /// How backends are probed while `health_check_interval_secs` > 0. Default: TCP connect.
    pub health_check: Option<HealthCheckInput>,
    /// How often upstream hostnames are resolved again, in seconds; 0 resolves them once.
    /// Default 30.
    pub dns_refresh_secs: Option<u64>,
    pub upstreams: Vec<UpstreamConfig>,
}

impl LoadBalancerConfig {
    /// Merge the config over [`AdaptiveLbOpt::default`], leaving unset fields at their defaults.
    fn to_lb_opt(&self, upstream_tls: &UpstreamTlsInput) -> Result<AdaptiveLbOpt> {
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
        if let Some(key) = &self.hash_key {
            opt.hash_key = key
                .parse()
                .map_err(|err| eyre!("load_balancer.hash_key: {err}"))?;
        }
        if let Some(secs) = self.dns_refresh_secs {
            opt.discovery_interval = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(check) = &self.health_check {
            if opt.health_check_interval.is_none() {
                return Err(eyre!(
                    "load_balancer.health_check is set but health_check_interval_secs is 0"
                ));
            }
            opt.health_check = check
                .to_opt(upstream_tls)
                .map_err(|err| eyre!("load_balancer.health_check: {err}"))?;
        }

        Ok(opt)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthCheckType {
    #[default]
    Tcp,
    Http,
}

/// Active health check (nginx Plus `health_check`, `match status=`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckInput {
    /// `tcp` (connect, the default) or `http` (GET `path`, expect a status).
    #[serde(rename = "type", default)]
    pub kind: HealthCheckType,
    /// http: path and query to request. Default `/`.
    pub path: Option<String>,
    /// http: `Host` header and SNI. Default: the route's `upstream_tls.sni`, else `localhost`.
    pub host: Option<String>,
    /// http: statuses counting as healthy. Default: any 2xx or 3xx.
    #[serde(default)]
    pub expected_status: Vec<u16>,
    /// http: connect and read timeout in milliseconds. Default 1000.
    pub timeout_ms: Option<u64>,
    /// http: probe this port instead of the backend's.
    pub port: Option<u16>,
    /// Successful probes in a row that bring a backend back. Default 1.
    pub passes: Option<usize>,
    /// Failed probes in a row that take a backend out. Default 1.
    pub fails: Option<usize>,
}

impl HealthCheckInput {
    /// The probe, over TLS (with its SNI and verification) when the route's upstream is.
    fn to_opt(&self, upstream_tls: &UpstreamTlsInput) -> Result<HealthCheckOpt> {
        let passes = self.passes.unwrap_or(1);
        let fails = self.fails.unwrap_or(1);
        if passes == 0 || fails == 0 {
            return Err(eyre!("passes and fails must be at least 1"));
        }
        let probe = match self.kind {
            HealthCheckType::Tcp => {
                let http_only = [
                    ("path", self.path.is_some()),
                    ("host", self.host.is_some()),
                    ("expected_status", !self.expected_status.is_empty()),
                    ("timeout_ms", self.timeout_ms.is_some()),
                    ("port", self.port.is_some()),
                ];
                if let Some((field, _)) = http_only.iter().find(|(_, set)| *set) {
                    return Err(eyre!("{field} only applies to type = \"http\""));
                }
                HealthProbe::Tcp
            }
            HealthCheckType::Http => {
                let path = self.path.clone().unwrap_or_else(|| "/".to_string());
                if !path.starts_with('/') || path.parse::<http::uri::PathAndQuery>().is_err() {
                    return Err(eyre!("path '{path}' must be an absolute path like /health"));
                }
                if let Some(code) = self
                    .expected_status
                    .iter()
                    .find(|c| !(100..=599).contains(*c))
                {
                    return Err(eyre!("expected_status {code} is not an HTTP status code"));
                }
                if self.timeout_ms == Some(0) {
                    return Err(eyre!("timeout_ms must be greater than 0"));
                }
                let host = self
                    .host
                    .clone()
                    .or_else(|| upstream_tls.sni.clone())
                    .unwrap_or_else(|| "localhost".to_string());
                HeaderValue::from_str(&host).wrap_err_with(|| format!("invalid host '{host}'"))?;
                let tls = upstream_tls.enabled.then(|| ProbeTls {
                    sni: upstream_tls.sni.clone().unwrap_or_else(|| host.clone()),
                    verify: upstream_tls.verify.unwrap_or(true),
                });
                HealthProbe::Http(HttpProbe {
                    path,
                    host,
                    expected_status: self.expected_status.clone(),
                    timeout: Duration::from_millis(self.timeout_ms.unwrap_or(1000)),
                    port: self.port,
                    tls,
                })
            }
        };
        Ok(HealthCheckOpt {
            probe,
            passes,
            fails,
        })
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
    use std::path::PathBuf;

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
    fn upstreams_may_be_hostnames_resolved_on_an_interval() {
        let config = toml(&MINIMAL_TOML.replace("127.0.0.1:4001", "api.internal:4001"));
        config.validate().unwrap();
        let lb = &config.proxy.router[0].load_balancer;
        let opt = lb.to_lb_opt(&Default::default()).unwrap();
        assert_eq!(opt.discovery_interval, Some(Duration::from_secs(30)));

        let config = with_lb("[proxy.router.load_balancer]\ndns_refresh_secs = 0");
        let lb = &config.proxy.router[0].load_balancer;
        assert_eq!(
            lb.to_lb_opt(&Default::default())
                .unwrap()
                .discovery_interval,
            None
        );
    }

    #[test]
    fn invalid_backend_address_is_an_error_not_a_panic() {
        let config = toml(&MINIMAL_TOML.replace("127.0.0.1:4001", "not-an-address"));
        assert!(errors_of(&config).contains("not-an-address"));
    }

    #[test]
    fn virtual_host_names_are_validated() {
        let with_host = |host: &str| {
            toml(&MINIMAL_TOML.replace(
                "path = \"/api/*\"",
                &format!("path = \"/api/*\"\nhost = \"{host}\""),
            ))
        };
        for good in [
            "example.com",
            "*.example.com",
            "localhost",
            "10.0.0.1",
            "::1",
        ] {
            with_host(good).validate().unwrap();
        }
        for bad in [
            "*",
            "*.",
            "www.*.com",
            "example.*",
            "a..b",
            "a b.com",
            "*example.com",
        ] {
            assert!(
                errors_of(&with_host(bad)).contains("must be a name"),
                "{bad}"
            );
        }
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
    fn pid_file_and_upgrade_sock_default_to_the_runtime_dir() {
        let config = toml(MINIMAL_TOML);
        let dir = default_runtime_dir();
        assert_eq!(
            PathBuf::from(config.server.pid_file()),
            dir.join(PID_FILE_NAME)
        );
        assert_eq!(
            PathBuf::from(config.server.upgrade_sock()),
            dir.join(UPGRADE_SOCK_NAME)
        );
        assert!(!config.server.upgrade_sock().starts_with("/tmp/"));

        let custom = toml(&format!(
            "[server]\npid_file = \"/run/x/r.pid\"\nupgrade_sock = \"/run/x/u.sock\"\n{MINIMAL_TOML}"
        ));
        assert_eq!(custom.server.pid_file(), "/run/x/r.pid");
        assert_eq!(custom.server.upgrade_sock(), "/run/x/u.sock");
    }

    #[test]
    fn too_long_upgrade_socket_path_is_rejected() {
        let long = format!("/run/{}/up.sock", "x".repeat(110));
        let config = toml(&format!(
            "[server]\nupgrade_sock = \"{long}\"\n{MINIMAL_TOML}"
        ));
        assert!(errors_of(&config).contains("server.upgrade_sock"));
    }

    #[test]
    fn a_taken_metrics_port_fails_fast_with_the_field_name() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = toml(&format!(
            "[server]\nprometheus_address = \"{}\"\n{}",
            taken.local_addr().unwrap(),
            MINIMAL_TOML.replace("listener = 3500", "listener = \"127.0.0.1:0\"")
        ));
        let err = config.into_server_builder().err().unwrap().to_string();
        assert!(err.contains("server.prometheus_address"), "{err}");
    }

    #[test]
    fn tls_certificate_problems_are_reported_by_validation() {
        let config = toml(&MINIMAL_TOML.replace(
            "listener = 3500",
            "listener = 3500\n[proxy.tls]\nlistener = 3443\ncert_path = \"/nonexistent/c.pem\"\nkey_path = \"/nonexistent/k.pem\"",
        ));
        let err = errors_of(&config);
        assert!(
            err.contains("proxy.tls: failed to read certificate"),
            "{err}"
        );

        let dir = crate::tls::test_certs::temp_dir("config");
        let (cert, key) = crate::tls::test_certs::write(&dir);
        let valid = toml(&MINIMAL_TOML.replace(
            "listener = 3500",
            &format!("listener = 3500\n[proxy.tls]\nlistener = 3443\ncert_path = \"{cert}\"\nkey_path = \"{key}\""),
        ));
        valid.validate().unwrap();
    }

    #[test]
    fn relative_tls_paths_resolve_against_the_config_dir() {
        let mut config = toml(&MINIMAL_TOML.replace(
            "listener = 3500",
            "listener = 3500\n[proxy.tls]\nlistener = 3443\ncert_path = \"certs/c.pem\"\nkey_path = \"/abs/k.pem\"\n\
             client_ca_path = \"ca.pem\"\n\
             [[proxy.tls.certificates]]\ncert_path = \"b.pem\"\nkey_path = \"b.key\"",
        ));
        config.resolve_paths(std::path::Path::new("/etc/routini"));
        let tls = config.proxy.tls.unwrap();
        assert_eq!(tls.cert_path, "/etc/routini/certs/c.pem");
        assert_eq!(tls.key_path, "/abs/k.pem");
        assert_eq!(tls.client_ca_path.as_deref(), Some("/etc/routini/ca.pem"));
        assert_eq!(tls.certificates[0].cert_path, "/etc/routini/b.pem");
        assert_eq!(tls.certificates[0].key_path, "/etc/routini/b.key");
    }

    #[test]
    fn tls_min_version_is_validated() {
        let dir = crate::tls::test_certs::temp_dir("min-version");
        let (cert, key) = crate::tls::test_certs::write(&dir);
        let with = |extra: &str| {
            toml(&MINIMAL_TOML.replace(
                "listener = 3500",
                &format!(
                    "listener = 3500\n[proxy.tls]\nlistener = 3443\ncert_path = \"{cert}\"\n\
                     key_path = \"{key}\"\n{extra}"
                ),
            ))
        };
        with("min_version = \"1.3\"").validate().unwrap();
        assert!(errors_of(&with("min_version = \"1.1\"")).contains("min_version must be"));
    }

    #[test]
    fn real_ip_is_parsed_and_validated() {
        let config = toml(&format!(
            "[server.real_ip]\ntrusted_proxies = [\"10.0.0.0/8\", \"127.0.0.1\"]\n{MINIMAL_TOML}"
        ));
        config.validate().unwrap();
        let real_ip = config.server.real_ip.unwrap().to_real_ip().unwrap();
        assert_eq!(real_ip.header, "x-forwarded-for");
        assert_eq!(real_ip.trusted.len(), 2);

        let config = toml(&format!(
            "[server.real_ip]\ntrusted_proxies = [\"10.0.0.300\"]\nheader = \"bad header\"\n\
             {MINIMAL_TOML}"
        ));
        let err = errors_of(&config);
        assert!(err.contains("10.0.0.300"), "{err}");

        let config = toml(&format!(
            "[server.real_ip]\ntrusted_proxies = []\n{MINIMAL_TOML}"
        ));
        assert!(errors_of(&config).contains("trusted_proxies is empty"));
    }

    /// The minimal config with `lb` added to its load_balancer table.
    fn with_lb(lb: &str) -> Config {
        toml(&MINIMAL_TOML.replace(
            "[[proxy.router.load_balancer.upstreams]]",
            &format!("{lb}\n[[proxy.router.load_balancer.upstreams]]"),
        ))
    }

    #[test]
    fn health_checks_are_parsed_with_upstream_tls() {
        let config = with_lb(
            "[proxy.router.load_balancer.health_check]\ntype = \"http\"\npath = \"/healthz?deep=1\"\n\
             expected_status = [200, 204]\nfails = 3\n\
             [proxy.router.upstream_tls]\nenabled = true\nsni = \"api.internal\"",
        );
        config.validate().unwrap();
        let route = &config.proxy.router[0];
        let opt = route.load_balancer.to_lb_opt(&route.upstream_tls).unwrap();
        assert_eq!(
            opt.health_check,
            HealthCheckOpt {
                probe: HealthProbe::Http(HttpProbe {
                    path: "/healthz?deep=1".into(),
                    host: "api.internal".into(),
                    expected_status: vec![200, 204],
                    timeout: Duration::from_secs(1),
                    port: None,
                    tls: Some(ProbeTls {
                        sni: "api.internal".into(),
                        verify: true,
                    }),
                }),
                passes: 1,
                fails: 3,
            }
        );
    }

    #[test]
    fn invalid_health_checks_are_reported() {
        for (table, expected) in [
            ("path = \"/x\"", "only applies to type = \"http\""),
            ("type = \"http\"\npath = \"health\"", "absolute path"),
            (
                "type = \"http\"\nexpected_status = [99]",
                "99 is not an HTTP status",
            ),
            ("type = \"http\"\ntimeout_ms = 0", "timeout_ms"),
            ("fails = 0", "at least 1"),
        ] {
            let config = with_lb(&format!(
                "[proxy.router.load_balancer.health_check]\n{table}"
            ));
            let err = errors_of(&config);
            assert!(err.contains(expected), "{table}: {err}");
        }
        let config = toml(&MINIMAL_TOML.replace(
            "[[proxy.router.load_balancer.upstreams]]",
            "[proxy.router.load_balancer]\nhealth_check_interval_secs = 0\n\
             health_check = { type = \"http\" }\n[[proxy.router.load_balancer.upstreams]]",
        ));
        let err = errors_of(&config);
        assert!(err.contains("health_check_interval_secs is 0"), "{err}");
    }

    #[test]
    fn client_timeouts_default_to_nginx_values() {
        let defaults = toml(MINIMAL_TOML).server.client_timeouts();
        assert_eq!(defaults, ClientTimeouts::default());
        assert_eq!(defaults.keepalive_secs, 75);
        assert_eq!(defaults.send, Duration::from_secs(60));

        let config = toml(&format!(
            "[server]\nkeepalive_timeout_secs = 0\nclient_body_timeout_secs = 5\n{MINIMAL_TOML}"
        ));
        config.validate().unwrap();
        let t = config.server.client_timeouts();
        assert_eq!((t.keepalive_secs, t.body), (0, Duration::from_secs(5)));

        let config = toml(&format!("[server]\nsend_timeout_secs = 0\n{MINIMAL_TOML}"));
        assert!(errors_of(&config).contains("send_timeout_secs must be greater than 0"));
    }

    #[test]
    fn invalid_regex_route_is_reported() {
        let config = toml(
            &MINIMAL_TOML.replace("path = \"/api/*\"", "path = \"^/(unclosed\"\nregex = true"),
        );
        assert!(errors_of(&config).contains("Invalid regex"));
    }
}
