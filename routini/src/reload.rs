//! In-process configuration reload (nginx `nginx -s reload`, but without restarting workers).
//!
//! The [RouteManager] owns every route: its adaptive load balancer, the background task running
//! discovery / health checks / strategy evaluation, and its hot-swappable [RouteState]. On SIGHUP
//! (or [RouteManager::reload]) the config file is re-read, validated and applied in place:
//!
//! | Change                                                | Effect                                    |
//! |-------------------------------------------------------|-------------------------------------------|
//! | upstreams of an existing route                        | swapped live, kept backends keep metrics and health |
//! | strategy, adaptive tuning, max_iterations, interval   | applied live to the running load balancer |
//! | headers, timeouts, retry, limits, auth, cache, ...    | route state swapped (limiter counters reset) |
//! | health check added, removed or changed (not interval) | that route is rebuilt (its metrics reset) |
//! | new / removed route                                   | started / stopped                         |
//! | `server`, `proxy.listener`, `proxy.tls`               | reported as requiring a restart or upgrade |
//! | TLS certificate files changed on disk                 | reported as requiring an upgrade (pingora's rustls listener can't swap them) |
//!
//! Routes are identified by `(host, regex, path)`. The routing table is rebuilt and swapped as a
//! whole, so the per-path lookup caches start fresh. A config that fails to parse or validate is
//! rejected as a whole and the running config stays.
//!
//! [RouteState]: crate::route::RouteState
use std::{collections::HashMap, fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use color_eyre::eyre::{Result, eyre};
use matchit::Router;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use regex::Regex;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::{Mutex, watch},
    task::JoinHandle,
};

use crate::{
    adaptive_loadbalancer::{
        AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine, options::AdaptiveLbConfig,
    },
    config::{Config, RouteEntry},
    load_balancing::{
        discovery::UpstreamDiscovery,
        strategy::{Adaptive, adaptive::AdaptiveStrategyMetrics},
    },
    proxy::{Proxy, RouteValue, RoutingTable},
    route::{RouteRuntime, SharedLb},
    server_builder::Route,
    tls,
    utils::{config_loader::load_config_from, constants::DEFAULT_PATH_CACHE_CAPACITY},
};

/// `(lowercased host, is_regex, transformed path)` — matches [RouteEntry::route_key].
pub type RouteKey = (String, bool, String);

/// How long stopping route tasks may take when the server shuts down.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

fn route_key(route: &Route) -> RouteKey {
    let host = route
        .host
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    (host, route.is_regex, route.path.clone())
}

/// `example.com/api/*`, `/api/*` or `~^/re` for log messages.
fn display_key((host, regex, path): &RouteKey) -> String {
    let path = path.replacen(crate::utils::constants::DEFAULT_WILDCARD_IDENTIFIER, "*", 1);
    let prefix = if *regex { "~" } else { "" };
    format!("{host}{prefix}{path}")
}

struct RouteTask {
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

struct ManagedRoute {
    key: RouteKey,
    runtime: Arc<RouteRuntime>,
    /// Handle to the discovery inside the load balancer, to change its upstreams.
    discovery: Arc<UpstreamDiscovery<AdaptiveStrategyMetrics>>,
    /// The strategy from the config, to tell a config change from a manual override.
    configured_strategy: Adaptive,
    task: Option<RouteTask>,
}

impl ManagedRoute {
    fn new(route: Route) -> Self {
        let key = route_key(&route);
        let decision_engine = AdaptiveDecisionEngine::new(&route.lb_options);
        let configured_strategy = route.lb_options.starting_strategy.clone();
        let lb: SharedLb = Arc::new(AdaptiveLoadBalancer::from_backends(
            route.backends,
            Some(route.lb_options),
            decision_engine,
        ));
        Self {
            key,
            runtime: Arc::new(RouteRuntime::new(lb, route.route_config)),
            discovery: route.discovery,
            configured_strategy,
            task: None,
        }
    }

    fn lb(&self) -> &SharedLb {
        &self.runtime.lb
    }

    /// Start the route's background service. Must be called from within the server runtime.
    fn spawn(&mut self) {
        if self.task.is_some() {
            return;
        }
        let (shutdown, rx) = watch::channel(false);
        let lb = self.lb().clone();
        let handle = tokio::spawn(async move { lb.start(rx).await });
        self.task = Some(RouteTask { shutdown, handle });
    }

    fn stop(&mut self) -> Option<JoinHandle<()>> {
        let task = self.task.take()?;
        let _ = task.shutdown.send(true);
        Some(task.handle)
    }
}

/// What a reload changed, by route.
#[derive(Debug, Default, PartialEq)]
pub struct ReloadReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    /// Routes rebuilt from scratch because their health check was added, removed or changed.
    pub rebuilt: Vec<String>,
    pub unchanged: Vec<String>,
    /// Settings that changed in the file but only take effect after a restart or upgrade.
    pub restart_required: Vec<String>,
}

impl Display for ReloadReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (label, routes) in [
            ("added", &self.added),
            ("removed", &self.removed),
            ("updated", &self.updated),
            ("rebuilt", &self.rebuilt),
            ("unchanged", &self.unchanged),
        ] {
            if !routes.is_empty() {
                parts.push(format!("{label}: {}", routes.join(" ")));
            }
        }
        if parts.is_empty() {
            write!(f, "no routes")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

struct State {
    routes: Vec<ManagedRoute>,
    /// Last applied config, to diff route settings and detect changes that need a restart.
    applied: Option<Config>,
    /// Hash of the TLS certificate and key the listener was started with. The listener can't
    /// swap them, so a reload only reports when they changed on disk.
    tls_fingerprint: Option<u64>,
    started: bool,
}

/// Owns the routes of a running server and applies config reloads. Runs as a pingora background
/// service, see the [module docs](self).
pub struct RouteManager {
    proxy: Proxy,
    config_path: Option<PathBuf>,
    state: Mutex<State>,
}

impl RouteManager {
    /// Build the routes and a proxy serving them. Route tasks start with the service. The
    /// returned [Proxy] shares its routing table with the manager; server-level proxy settings
    /// (access log, compression, ...) are set on it by the caller.
    pub(crate) fn new(
        routes: Vec<Route>,
        config_path: Option<PathBuf>,
        applied: Option<Config>,
    ) -> Result<(Self, Proxy)> {
        let routes: Vec<ManagedRoute> = routes.into_iter().map(ManagedRoute::new).collect();
        let proxy = Proxy::with_table(build_table(&routes)?);
        let tls_fingerprint = applied
            .as_ref()
            .and_then(|c| c.proxy.tls.as_ref())
            .and_then(|tls| tls::fingerprint(&tls.cert_path, &tls.key_path));
        let manager = Self {
            proxy: proxy.clone(),
            config_path,
            state: Mutex::new(State {
                routes,
                applied,
                tls_fingerprint,
                started: false,
            }),
        };
        Ok((manager, proxy))
    }

    /// Whether the service has started its routes and listens for SIGHUP.
    pub async fn is_running(&self) -> bool {
        self.state.lock().await.started
    }

    /// The load balancer currently serving the default-server route with config `path` (as
    /// written in the config, e.g. `/api/*`), if any.
    pub async fn load_balancer(&self, path: &str) -> Option<SharedLb> {
        self.load_balancer_for(None, path).await
    }

    /// Like [Self::load_balancer] for a route on virtual host `host`.
    pub async fn load_balancer_for(&self, host: Option<&str>, path: &str) -> Option<SharedLb> {
        let host = host.map(str::to_ascii_lowercase).unwrap_or_default();
        let path = path.replacen('*', crate::utils::constants::DEFAULT_WILDCARD_IDENTIFIER, 1);
        let state = self.state.lock().await;
        state
            .routes
            .iter()
            .find(|r| r.key.0 == host && !r.key.1 && r.key.2 == path)
            .map(|r| r.lb().clone())
    }

    /// Re-read the config file the server was started with and apply it.
    pub async fn reload_from_file(&self) -> Result<ReloadReport> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| eyre!("Server was not started from a config file, nothing to reload"))?;
        self.reload(load_config_from(path)?).await
    }

    /// Apply `config`. It is validated first; on error nothing changes.
    pub async fn reload(&self, config: Config) -> Result<ReloadReport> {
        config.validate()?;
        let new_routes = config.routes()?;

        let mut state = self.state.lock().await;
        let started = state.started;
        let mut report = ReloadReport::default();

        let old_entries: HashMap<RouteKey, RouteEntry> = state
            .applied
            .iter()
            .flat_map(|c| c.proxy.router.iter())
            .map(|e| (e.route_key(), e.clone()))
            .collect();
        let mut previous: HashMap<RouteKey, ManagedRoute> =
            state.routes.drain(..).map(|r| (r.key.clone(), r)).collect();
        let mut next = Vec::with_capacity(new_routes.len());

        for (route, entry) in new_routes.into_iter().zip(&config.proxy.router) {
            let key = route_key(&route);
            let name = display_key(&key);
            let opts = &route.lb_options;
            let health_check = opts.health_check_interval.map(|_| &opts.health_check);

            let mut managed = match previous.remove(&key) {
                Some(mut existing) if existing.lb().health_check() == health_check => {
                    if update_in_place(&mut existing, route, old_entries.get(&key), entry).await {
                        report.updated.push(name);
                    } else {
                        report.unchanged.push(name);
                    }
                    existing
                }
                Some(mut existing) => {
                    existing.stop();
                    report.rebuilt.push(name);
                    new_route(route).await
                }
                None => {
                    report.added.push(name);
                    new_route(route).await
                }
            };
            if started {
                managed.spawn();
            }
            next.push(managed);
        }

        // Validation inserted every path into scratch routers, so this cannot conflict
        self.proxy.replace_routes(build_table(&next)?);

        for (key, mut removed) in previous {
            removed.stop();
            report.removed.push(display_key(&key));
        }

        state.routes = next;
        if let Some(applied) = &state.applied {
            for (section, changed) in [
                ("server", applied.server != config.server),
                (
                    "proxy.listener",
                    applied.proxy.listener != config.proxy.listener,
                ),
                ("proxy.tls", applied.proxy.tls != config.proxy.tls),
            ] {
                if changed {
                    report.restart_required.push(section.to_string());
                }
            }
        }
        let same_tls_files = state
            .applied
            .as_ref()
            .is_some_and(|applied| applied.proxy.tls == config.proxy.tls);
        if let Some(tls) = &config.proxy.tls {
            if same_tls_files
                && tls::fingerprint(&tls.cert_path, &tls.key_path) != state.tls_fingerprint
            {
                report
                    .restart_required
                    .push("proxy.tls certificate files changed on disk".to_string());
            }
        }
        state.applied = Some(config);
        Ok(report)
    }

    async fn reload_and_log(&self) {
        match self.reload_from_file().await {
            Ok(report) => {
                tracing::info!("Configuration reloaded: {report}");
                if !report.restart_required.is_empty() {
                    tracing::warn!(
                        sections = report.restart_required.join(" "),
                        "Changes in these sections only take effect after a restart"
                    );
                }
            }
            Err(err) => {
                tracing::error!("Configuration reload failed, keeping current config: {err:#}")
            }
        }
    }
}

/// Apply a new [Route] definition to a running route. Returns whether anything changed.
async fn update_in_place(
    existing: &mut ManagedRoute,
    route: Route,
    old_entry: Option<&RouteEntry>,
    new_entry: &RouteEntry,
) -> bool {
    let mut changed = false;
    let lb = existing.lb().clone();

    let new_backends = route.discovery.get();
    if existing.discovery.get() != new_backends {
        existing.discovery.set(new_backends);
        if let Err(err) = lb.refresh().await {
            tracing::error!(
                route = display_key(&existing.key),
                "Failed to refresh backends: {err}"
            );
        }
        changed = true;
    }

    let options = &route.lb_options;
    if **lb.config() != AdaptiveLbConfig::from(options)
        || *lb.decision_engine().thresholds() != options.into()
    {
        lb.reconfigure(options);
        changed = true;
    }

    // Only a change in the file overrides the strategy, so a manual override survives reloads
    // that don't touch it.
    if options.starting_strategy != existing.configured_strategy {
        lb.update_strategy(options.starting_strategy.clone()).await;
        existing.configured_strategy = options.starting_strategy.clone();
        changed = true;
    }

    // Swapping the route state resets its rate/connection limiter and passive-health counters,
    // so only do it when a route setting (anything but the load balancer) actually changed.
    let route_settings_changed = match old_entry {
        Some(old) => {
            let mut old = old.clone();
            old.load_balancer = new_entry.load_balancer.clone();
            old != *new_entry
        }
        None => true,
    };
    if route_settings_changed {
        existing.runtime.reload(route.route_config);
        changed = true;
    }
    changed
}

/// Build a route and populate its backends, so it can serve as soon as it is routable.
async fn new_route(route: Route) -> ManagedRoute {
    let managed = ManagedRoute::new(route);
    if let Err(err) = managed.lb().refresh().await {
        tracing::error!(
            route = display_key(&managed.key),
            "Failed to load backends: {err}"
        );
    }
    managed
}

fn build_table(routes: &[ManagedRoute]) -> Result<RoutingTable> {
    let mut default_router = Router::new();
    let mut default_regex = Vec::new();
    let mut vhosts: HashMap<String, Router<RouteValue>> = HashMap::new();
    for route in routes {
        let value = RouteValue {
            runtime: route.runtime.clone(),
        };
        let (host, is_regex, path) = &route.key;
        let name = display_key(&route.key);
        if *is_regex {
            let re = Regex::new(path).map_err(|err| eyre!("route '{name}': {err}"))?;
            default_regex.push((re, value));
            continue;
        }
        let router = if host.is_empty() {
            &mut default_router
        } else {
            vhosts.entry(host.clone()).or_insert_with(Router::new)
        };
        router
            .insert(path.clone(), value)
            .map_err(|err| eyre!("route '{name}': {err}"))?;
    }
    Ok(RoutingTable::new(
        default_router,
        default_regex,
        vhosts,
        DEFAULT_PATH_CACHE_CAPACITY,
    ))
}

#[async_trait]
impl BackgroundService for RouteManager {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        // Install the handler before reporting started, so a SIGHUP after is_running() never
        // hits the default action (terminate)
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(signal) => Some(signal),
            Err(err) => {
                tracing::error!("Failed to install SIGHUP handler, config reload disabled: {err}");
                None
            }
        };

        {
            let mut state = self.state.lock().await;
            for route in &mut state.routes {
                tracing::info!(route = display_key(&route.key), "Starting route");
                route.spawn();
            }
            state.started = true;
        }

        while !*shutdown.borrow() {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                Some(_) = async { hangup.as_mut()?.recv().await } => {
                    tracing::info!("SIGHUP received, reloading configuration");
                    self.reload_and_log().await;
                }
            }
        }

        let handles: Vec<_> = {
            let mut state = self.state.lock().await;
            state
                .routes
                .iter_mut()
                .filter_map(ManagedRoute::stop)
                .collect()
        };
        let _ = tokio::time::timeout(STOP_TIMEOUT, futures::future::join_all(handles)).await;
    }
}
