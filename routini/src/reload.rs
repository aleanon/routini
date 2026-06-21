//! Hot config reload on `SIGHUP` (nginx `nginx -s reload`).
//!
//! On `SIGHUP` the config file is re-read and each route's hot-swappable [`RouteState`] is rebuilt
//! and atomically replaced. This covers per-route tunables (headers, timeouts, retry, limits,
//! auth, cache, actions, ...). Structural changes — adding/removing routes, changing upstreams or
//! the load-balancing strategy — cannot be applied in place (their background services are wired at
//! startup) and require Pingora's zero-downtime graceful restart instead; such entries are counted
//! as "skipped".
//!
//! [`RouteState`]: crate::route::RouteState
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use color_eyre::eyre::Result;
use signal_hook::consts::SIGHUP;
use signal_hook::iterator::Signals;

use crate::route::RouteRuntime;
use crate::utils::config_loader::load_config_from;

/// `(lowercased host, is_regex, transformed path)` — matches `RouteEntry::route_key`.
pub type RouteKey = (String, bool, String);
/// Maps each configured route to its live runtime so reloads can target the right one.
pub type RouteRegistry = HashMap<RouteKey, Arc<RouteRuntime>>;

/// Spawn a background thread that reloads `config_path` whenever `SIGHUP` is received.
pub fn spawn_reload_watcher(config_path: String, registry: Arc<RouteRegistry>) {
    thread::spawn(move || {
        let mut signals = match Signals::new([SIGHUP]) {
            Ok(signals) => signals,
            Err(err) => {
                tracing::error!("Failed to register SIGHUP handler: {err}");
                return;
            }
        };
        tracing::info!("SIGHUP config reload enabled (config: {config_path})");
        for _ in signals.forever() {
            match reload(&config_path, &registry) {
                Ok((applied, skipped)) => tracing::info!(
                    "Config reloaded: {applied} route(s) updated, {skipped} skipped \
                     (new/removed routes or backend/strategy changes need a restart)"
                ),
                Err(err) => tracing::error!("Config reload failed, keeping current config: {err}"),
            }
        }
    });
}

/// Re-read the config and swap in fresh per-route state. Returns `(applied, skipped)`.
pub fn reload(config_path: &str, registry: &RouteRegistry) -> Result<(usize, usize)> {
    let config = load_config_from(config_path)?;
    let mut applied = 0;
    let mut skipped = 0;
    for entry in &config.proxy.router {
        match registry.get(&entry.route_key()) {
            Some(runtime) => {
                runtime.reload(entry.route_config()?);
                applied += 1;
            }
            None => skipped += 1,
        }
    }
    Ok((applied, skipped))
}
