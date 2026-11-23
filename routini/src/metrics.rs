//! Prometheus metrics per route and backend, served with pingora's own on
//! `server.prometheus_address`.
//!
//! Counters and histograms are updated per request. Backend state (health, active connections,
//! latency average) and each route's current strategy are read from the live routing table
//! when scraped, so routes and backends removed by a reload disappear instead of leaving stale
//! series behind.
//!
//! `route` labels are the route as written in the config (`/api/*`, `api.example.com/v1/*`,
//! `~\.png$` for regex routes), or `-` for requests no route matched. `backend` is `ip:port`.

use std::sync::{Arc, LazyLock, Mutex, Weak};

use arc_swap::ArcSwap;
use pingora::listeners::AcceptStats;
use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, Opts,
    core::{Collector, Desc},
    proto::MetricFamily,
};

use crate::{
    load_balancing::{Metrics, strategy::Adaptive},
    proxy::RoutingTable,
};

/// Route label for requests that matched no route.
pub const NO_ROUTE: &str = "-";

pub static REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register(IntCounterVec::new(
        Opts::new(
            "routini_requests_total",
            "Requests answered, by route and status",
        ),
        &["route", "status"],
    ))
});

pub static REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    register(HistogramVec::new(
        HistogramOpts::new(
            "routini_request_duration_seconds",
            "Time from receiving a request to finishing its response",
        ),
        &["route"],
    ))
});

pub static UPSTREAM_RESPONSE: LazyLock<HistogramVec> = LazyLock::new(|| {
    register(HistogramVec::new(
        HistogramOpts::new(
            "routini_upstream_response_seconds",
            "Time from sending a request to a backend to receiving its response header",
        ),
        &["route", "backend"],
    ))
});

pub static UPSTREAM_CONNECT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register(HistogramVec::new(
        HistogramOpts::new(
            "routini_upstream_connect_seconds",
            "Time to open a new connection to a backend: the TCP connect, then the TLS handshake",
        ),
        &["route", "backend", "layer"],
    ))
});

/// Record how long each layer of a new backend connection took to establish (pingora's
/// timing digest: TCP first, then TLS).
pub fn upstream_connected(route: &str, backend: &str, digest: &pingora::protocols::Digest) {
    for (layer, timing) in ["tcp", "tls"].into_iter().zip(&digest.timing_digest) {
        if let Some(took) = timing.as_ref().and_then(|t| t.establishment_duration) {
            UPSTREAM_CONNECT
                .with_label_values(&[route, backend, layer])
                .observe(took.as_secs_f64());
        }
    }
}

pub static UPSTREAM_FAILURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register(IntCounterVec::new(
        Opts::new(
            "routini_upstream_failures_total",
            "Failed attempts at a backend: connect, timeout, status (retried on) or error",
        ),
        &["route", "backend", "kind"],
    ))
});

fn register<C: Collector + Clone + 'static>(collector: prometheus::Result<C>) -> C {
    let collector = collector.expect("valid metric definition");
    // Only fails if registered twice, which LazyLock prevents
    let _ = prometheus::register(Box::new(collector.clone()));
    collector
}

/// Record a failed attempt at `backend` of `route`.
pub fn upstream_failure(route: &str, backend: &str, kind: &str) {
    UPSTREAM_FAILURES
        .with_label_values(&[route, backend, kind])
        .inc();
}

/// Routing tables of the running proxies, read at scrape time. Weak so a dropped proxy (e.g.
/// in tests) stops being reported.
static TABLES: LazyLock<Mutex<Vec<Weak<ArcSwap<RoutingTable>>>>> = LazyLock::new(|| {
    let _ = prometheus::register(Box::new(LiveState::new()));
    Mutex::default()
});

/// Report the routes in `table` (and every table it is swapped to) at scrape time.
pub fn watch(table: &Arc<ArcSwap<RoutingTable>>) {
    let mut tables = TABLES.lock().unwrap_or_else(|p| p.into_inner());
    tables.retain(|t| t.strong_count() > 0);
    tables.push(Arc::downgrade(table));
}

/// Accept counters of the proxy listeners, by listener name (`http`, `https`).
type Listeners = Mutex<Vec<(&'static str, Weak<AcceptStats>)>>;
static LISTENERS: LazyLock<Listeners> = LazyLock::new(|| {
    // The collector reports listeners too; make sure it is registered
    LazyLock::force(&TABLES);
    Mutex::default()
});

/// Report `stats` as the listener `name` at scrape time. Listeners of the same name (several
/// servers in one process, as in tests) are summed.
pub fn watch_listener(name: &'static str, stats: &Arc<AcceptStats>) {
    let mut listeners = LISTENERS.lock().unwrap_or_else(|p| p.into_inner());
    listeners.retain(|(_, s)| s.strong_count() > 0);
    listeners.push((name, Arc::downgrade(stats)));
}

/// Collects backend and strategy gauges from the live routing tables, and listener counters.
struct LiveState {
    descs: Vec<Desc>,
}

const HEALTHY: &str = "routini_backend_healthy";
const ACTIVE: &str = "routini_backend_active_connections";
const LATENCY: &str = "routini_backend_latency_ewma_seconds";
const STRATEGY: &str = "routini_route_strategy";
const OPEN: &str = "routini_listener_open_connections";
const PENDING: &str = "routini_listener_pending_handshakes";
const CLOSED: &str = "routini_listener_closed_total";

const OPEN_HELP: &str = "Connections accepted and still open";
const PENDING_HELP: &str = "Connections whose TLS handshake hasn't finished";
const CLOSED_HELP: &str = "Connections closed by the listener without serving them: at \
     connection_limit or handshake_limit, peer_gone before its handshake, handshake_timeout or \
     handshake_error";

fn gauge_family(name: &str, help: &str) -> MetricFamily {
    family(name, help, prometheus::proto::MetricType::GAUGE)
}

fn family(name: &str, help: &str, kind: prometheus::proto::MetricType) -> MetricFamily {
    let mut family = MetricFamily::default();
    family.set_name(name.to_string());
    family.set_help(help.to_string());
    family.set_field_type(kind);
    family
}

fn labels(labels: &[(&str, &str)]) -> Vec<prometheus::proto::LabelPair> {
    labels
        .iter()
        .map(|(name, value)| {
            let mut pair = prometheus::proto::LabelPair::default();
            pair.set_name(name.to_string());
            pair.set_value(value.to_string());
            pair
        })
        .collect()
}

fn counter_sample(label_pairs: &[(&str, &str)], value: u64) -> prometheus::proto::Metric {
    let mut metric = prometheus::proto::Metric::default();
    metric.set_label(labels(label_pairs));
    let mut counter = prometheus::proto::Counter::default();
    counter.set_value(value as f64);
    metric.set_counter(counter);
    metric
}

fn sample(label_pairs: &[(&str, &str)], value: f64) -> prometheus::proto::Metric {
    let mut metric = prometheus::proto::Metric::default();
    metric.set_label(labels(label_pairs));
    let mut gauge = prometheus::proto::Gauge::default();
    gauge.set_value(value);
    metric.set_gauge(gauge);
    metric
}

impl LiveState {
    fn new() -> Self {
        let desc = |name: &str, help: &str, labels: &[&str]| {
            Desc::new(
                name.to_string(),
                help.to_string(),
                labels.iter().map(|l| l.to_string()).collect(),
                Default::default(),
            )
            .expect("valid metric description")
        };
        Self {
            descs: vec![
                desc(
                    HEALTHY,
                    "Whether the backend is in rotation",
                    &["route", "backend"],
                ),
                desc(
                    ACTIVE,
                    "Requests in flight to the backend",
                    &["route", "backend"],
                ),
                desc(
                    LATENCY,
                    "Moving average of the backend's response latency",
                    &["route", "backend"],
                ),
                desc(
                    STRATEGY,
                    "The strategy a route currently selects backends with (1)",
                    &["route", "strategy"],
                ),
                desc(OPEN, OPEN_HELP, &["listener"]),
                desc(PENDING, PENDING_HELP, &["listener"]),
                desc(CLOSED, CLOSED_HELP, &["listener", "reason"]),
            ],
        }
    }
}

impl Collector for LiveState {
    fn desc(&self) -> Vec<&Desc> {
        self.descs.iter().collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut healthy = gauge_family(HEALTHY, "Whether the backend is in rotation");
        let mut active = gauge_family(ACTIVE, "Requests in flight to the backend");
        let mut latency = gauge_family(LATENCY, "Moving average of the backend's response latency");
        let mut strategy = gauge_family(
            STRATEGY,
            "The strategy a route currently selects backends with (1)",
        );

        let tables: Vec<_> = {
            let tables = TABLES.lock().unwrap_or_else(|p| p.into_inner());
            tables.iter().filter_map(Weak::upgrade).collect()
        };
        for table in tables {
            let table = table.load();
            for runtime in table.routes() {
                let route = &*runtime.name;
                let lb = &runtime.lb;
                for backend in lb.backends().iter() {
                    let addr = backend.addr.to_string();
                    let labels = [("route", route), ("backend", addr.as_str())];
                    let up = lb.backend_ready(backend);
                    healthy
                        .mut_metric()
                        .push(sample(&labels, f64::from(u8::from(up))));
                    if let Some(n) = backend.metrics.active_connections() {
                        active.mut_metric().push(sample(&labels, n as f64));
                    }
                    // Recorded in milliseconds
                    if let Some(ms) = backend.metrics.average_latency() {
                        latency
                            .mut_metric()
                            .push(sample(&labels, f64::from(ms) / 1000.0));
                    }
                }
                // Skipped for this scrape if a strategy switch holds the lock right now
                if let Some(current) = lb.try_current_strategy() {
                    let name = strategy_name(&current);
                    strategy
                        .mut_metric()
                        .push(sample(&[("route", route), ("strategy", name)], 1.0));
                }
            }
        }
        let mut families = vec![healthy, active, latency, strategy];
        families.extend(listener_families());
        families
    }
}

/// Totals per listener name, in first-registered order.
#[derive(Default)]
struct ListenerTotals {
    open: usize,
    pending: usize,
    closed: [u64; 5],
}

const CLOSE_REASONS: [&str; 5] = [
    "connection_limit",
    "handshake_limit",
    "peer_gone",
    "handshake_timeout",
    "handshake_error",
];

fn listener_families() -> Vec<MetricFamily> {
    let mut totals: Vec<(&'static str, ListenerTotals)> = Vec::new();
    {
        let listeners = LISTENERS.lock().unwrap_or_else(|p| p.into_inner());
        for (name, stats) in listeners.iter() {
            let Some(stats) = stats.upgrade() else {
                continue;
            };
            let index = match totals.iter().position(|(n, _)| n == name) {
                Some(index) => index,
                None => {
                    totals.push((name, ListenerTotals::default()));
                    totals.len() - 1
                }
            };
            let t = &mut totals[index].1;
            t.open += stats.open_connections();
            t.pending += stats.pending_handshakes();
            for (sum, value) in t.closed.iter_mut().zip([
                stats.closed_at_connection_limit(),
                stats.closed_at_handshake_limit(),
                stats.closed_peer_gone(),
                stats.handshake_timeouts(),
                stats.handshake_errors(),
            ]) {
                *sum += value;
            }
        }
    }
    if totals.is_empty() {
        return Vec::new();
    }
    let mut open = gauge_family(OPEN, OPEN_HELP);
    let mut pending = gauge_family(PENDING, PENDING_HELP);
    let mut closed = family(CLOSED, CLOSED_HELP, prometheus::proto::MetricType::COUNTER);
    for (name, t) in &totals {
        let listener = [("listener", *name)];
        open.mut_metric().push(sample(&listener, t.open as f64));
        pending
            .mut_metric()
            .push(sample(&listener, t.pending as f64));
        for (reason, value) in CLOSE_REASONS.iter().zip(t.closed) {
            closed.mut_metric().push(counter_sample(
                &[("listener", name), ("reason", reason)],
                value,
            ));
        }
    }
    vec![open, pending, closed]
}

fn strategy_name(strategy: &Adaptive) -> &'static str {
    match strategy {
        Adaptive::RoundRobin => "RoundRobin",
        Adaptive::Random => "Random",
        Adaptive::FNVHash => "FNVHash",
        Adaptive::Consistent => "Consistent",
        Adaptive::FewestConnections => "FewestConnections",
        Adaptive::FastestServer => "FastestServer",
    }
}
