//! Declarative configuration for the proxy, deserialized from `config.json`.
//!
//! The shapes here mirror `config.json` and convert into the builder types in
//! [`crate::server_builder`], so `main` can construct the whole server from a file instead of
//! hard-coded values.
use std::time::Duration;

use color_eyre::eyre::Result;
use serde::Deserialize;

use crate::{
    adaptive_loadbalancer::options::AdaptiveLbOpt,
    load_balancing::strategy::Adaptive,
    server_builder::{Route, RouteConfig, TlsConfig as BuilderTlsConfig},
};

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
    #[serde(default = "default_true")]
    pub strip_prefix: bool,
    pub load_balancer: LoadBalancerConfig,
}

impl RouteEntry {
    fn to_route(&self) -> Result<Route> {
        let upstreams = self
            .load_balancer
            .upstreams
            .iter()
            .map(|u| u.address.clone())
            .collect::<Vec<_>>();

        let route = Route::with_options(&self.path, upstreams, self.load_balancer.to_lb_opt())?
            .route_config(RouteConfig {
                strip_path_prefix: self.strip_prefix,
            });
        Ok(route)
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
    /// Currently informational: the route builder treats every backend with equal weight.
    #[serde(default = "default_weight")]
    pub weight: usize,
}
