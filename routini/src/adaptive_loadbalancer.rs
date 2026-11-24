pub mod background_service;
pub mod decision_engine;
pub mod options;

use std::{collections::BTreeSet, sync::Arc};

use arc_swap::{ArcSwap, Guard};
use pingora::connectors::{ConnectorOptions, TransportConnector, http::Connector as HttpConnector};
use tokio::sync::Notify;

use crate::{
    adaptive_loadbalancer::{
        decision_engine::DecisionEngine,
        options::{AdaptiveLbConfig, AdaptiveLbOpt, HealthCheckOpt, HealthProbe},
    },
    load_balancing::{
        Backend, Backends, LoadBalancer,
        health_check::{HealthCheck, HttpHealthCheck, TcpHealthCheck},
        strategy::{Adaptive, adaptive::AdaptiveStrategyMetrics},
    },
};

/// The metrics type the adaptive load balancer pins its backends to.
pub type AdaptiveBackend = Backend<AdaptiveStrategyMetrics>;
/// A [`Backends`] collection pinned to the adaptive metrics type.
pub type AdaptiveBackends = Backends<AdaptiveStrategyMetrics>;

/// `connector`: the options probes connect with, see
/// [AdaptiveLoadBalancer::from_backends_with_connector].
fn build_health_check(
    opt: &HealthCheckOpt,
    connector: Option<&ConnectorOptions>,
) -> Box<dyn HealthCheck<AdaptiveStrategyMetrics> + Send + Sync + 'static> {
    match &opt.probe {
        HealthProbe::Tcp => {
            let mut check = TcpHealthCheck::new();
            check.consecutive_success = opt.passes;
            check.consecutive_failure = opt.fails;
            if let Some(options) = connector {
                check.set_connector(TransportConnector::new(Some(options.clone())));
            }
            check
        }
        HealthProbe::Http(probe) => {
            let mut check = HttpHealthCheck::new(&probe.host, probe.tls.is_some());
            if let Some(options) = connector {
                check.set_connector(HttpConnector::new(Some(options.clone())));
            }
            check.consecutive_success = opt.passes;
            check.consecutive_failure = opt.fails;
            check.port_override = probe.port;
            // Validated as a path when the config was read
            if let Ok(mut req) =
                pingora::http::RequestHeader::build("GET", probe.path.as_bytes(), None)
            {
                let _ = req.append_header("Host", &probe.host);
                let _ = req.append_header("User-Agent", "routini-health-check");
                check.req = req;
            }
            let peer = &mut check.peer_template;
            peer.options.connection_timeout = Some(probe.timeout);
            peer.options.read_timeout = Some(probe.timeout);
            if let Some(tls) = &probe.tls {
                peer.sni = tls.sni.clone();
                peer.options.verify_cert = tls.verify;
                peer.options.verify_hostname = tls.verify;
                // Loaded and checked when the config was validated
                match tls
                    .ca_path
                    .as_deref()
                    .map(crate::tls::load_upstream_ca)
                    .transpose()
                {
                    Ok(ca) => peer.options.ca = ca,
                    Err(err) => tracing::error!("health check: {err:#}"),
                }
                match tls
                    .client_cert
                    .as_ref()
                    .map(crate::tls::load_client_cert)
                    .transpose()
                {
                    Ok(cert) => peer.client_cert_key = cert,
                    Err(err) => tracing::error!("health check: {err:#}"),
                }
            }
            let expected = probe.expected_status.clone();
            check.validator = Some(Box::new(move |resp| {
                let status = resp.status.as_u16();
                let healthy = if expected.is_empty() {
                    (200..400).contains(&status)
                } else {
                    expected.contains(&status)
                };
                if healthy {
                    Ok(())
                } else {
                    pingora::Error::e_explain(
                        pingora::ErrorType::CustomCode("unexpected status", status),
                        "during http health check",
                    )
                }
            }));
            Box::new(check)
        }
    }
}

pub struct AdaptiveLoadBalancer<D> {
    lb: LoadBalancer<Adaptive, AdaptiveStrategyMetrics>,
    decision_engine: D,
    /// Swapped as a whole by [Self::reconfigure], read on every request.
    config: ArcSwap<AdaptiveLbConfig>,
    /// The health check installed at construction. Health checks can't be added, removed or
    /// replaced afterwards, a reload that changes this rebuilds the load balancer.
    health_check: Option<HealthCheckOpt>,
    /// Wakes the background service after [Self::reconfigure] so new intervals apply at once.
    reconfigured: Notify,
}

impl<D: DecisionEngine> AdaptiveLoadBalancer<D> {
    pub fn from_backends(
        backends: AdaptiveBackends,
        options: Option<AdaptiveLbOpt>,
        decision_engine: D,
    ) -> Self {
        Self::from_backends_with_connector(backends, options, decision_engine, None)
    }

    /// Like [Self::from_backends], with health checks connecting with `connector` (key log,
    /// CA file, source addresses) instead of pingora's defaults. Routes built by the
    /// [ServerBuilder](crate::server_builder::ServerBuilder) get the server's, as proxied
    /// requests do.
    pub fn from_backends_with_connector(
        backends: AdaptiveBackends,
        options: Option<AdaptiveLbOpt>,
        decision_engine: D,
        connector: Option<&ConnectorOptions>,
    ) -> Self {
        let options = options.unwrap_or_default();
        let mut lb =
            LoadBalancer::from_backends_with_strategy(backends, options.starting_strategy.clone());

        let health_check = options
            .health_check_interval
            .is_some()
            .then(|| options.health_check.clone());
        if let Some(health_check) = &health_check {
            lb.set_health_check(build_health_check(health_check, connector));
            lb.set_health_check_mandatory(health_check.mandatory);
            lb.health_check_frequency = options.health_check_interval;
        }

        Self {
            lb,
            decision_engine,
            config: ArcSwap::from_pointee(AdaptiveLbConfig::from(options)),
            health_check,
            reconfigured: Notify::new(),
        }
    }

    /// The current runtime configuration.
    pub fn config(&self) -> Guard<Arc<AdaptiveLbConfig>> {
        self.config.load()
    }

    pub fn health_check_enabled(&self) -> bool {
        self.health_check.is_some()
    }

    /// The health check installed at construction, if any.
    pub fn health_check(&self) -> Option<&HealthCheckOpt> {
        self.health_check.as_ref()
    }

    /// Apply new tuning (thresholds, intervals, max iterations, smoothing factor) without
    /// touching backends, metrics or the current strategy. Toggling health checks on or off is
    /// not possible here, see [Self::health_check_enabled].
    pub fn reconfigure(&self, options: &AdaptiveLbOpt) {
        self.decision_engine.reconfigure(options);
        self.config.store(Arc::new(AdaptiveLbConfig::from(options)));
        self.reconfigured.notify_one();
    }

    /// Re-run service discovery now and rebuild the selector if the backends changed.
    /// Backends that are kept retain their metrics and health.
    pub async fn refresh(&self) -> pingora::Result<()> {
        self.lb.update().await
    }

    /// Probe every backend once now, outside the background task's schedule.
    pub async fn run_health_check(&self) {
        self.lb
            .backends()
            .run_health_check(self.lb.parallel_health_check)
            .await
    }

    pub(crate) async fn wait_reconfigured(&self) {
        self.reconfigured.notified().await
    }

    pub fn backends(&self) -> Arc<BTreeSet<AdaptiveBackend>> {
        self.lb.backends().get_backend()
    }

    pub fn select(&self, key: &[u8]) -> Option<AdaptiveBackend> {
        self.lb.select(key, self.config.load().max_iterations)
    }

    /// Select a healthy backend for `key` (see [options::HashKey]) whose address is not in
    /// `exclude`. Used for per-request failover so a retry lands on a different backend than the
    /// one that just failed; hash strategies then continue to the key's next backend.
    pub fn select_excluding(
        &self,
        key: &[u8],
        exclude: &[pingora::protocols::l4::socket::SocketAddr],
    ) -> Option<AdaptiveBackend> {
        self.lb.select_with(
            key,
            self.config.load().max_iterations,
            |backend, healthy| healthy && !exclude.iter().any(|addr| addr == &backend.addr),
        )
    }

    /// Manually enable/disable a backend (used by passive health checks to eject/restore).
    pub fn set_backend_enabled(&self, backend: &AdaptiveBackend, enabled: bool) {
        self.lb.backends().set_enable(backend, enabled);
    }

    /// Whether `backend` is healthy and enabled, i.e. selectable.
    pub fn backend_ready(&self, backend: &AdaptiveBackend) -> bool {
        self.lb.backends().ready(backend)
    }

    /// [Self::current_strategy] without waiting, `None` while a switch is in progress.
    pub fn try_current_strategy(&self) -> Option<Adaptive> {
        self.lb.try_current_strategy()
    }

    /// The strategy the load balancer is currently selecting backends with.
    pub async fn current_strategy(&self) -> Adaptive {
        self.lb.current_strategy().await
    }

    pub fn decision_engine(&self) -> &D {
        &self.decision_engine
    }

    /// Manually override the strategy. It also becomes the decision engine's default, so the
    /// engine returns to it (instead of the configured starting strategy) whenever the backends
    /// are balanced. Returns false if the strategy was already in use.
    pub async fn update_strategy(&self, new_strategy: Adaptive) -> bool
    where
        D: DecisionEngine<Strategy = Adaptive>,
    {
        self.decision_engine
            .set_default_strategy(new_strategy.clone());
        self.lb.update_strategy(new_strategy).await
    }
}
