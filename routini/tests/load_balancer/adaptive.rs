//! End-to-end tests of adaptive strategy switching through the real background service.
use std::{sync::Arc, time::Duration};

use pingora::services::background::BackgroundService;
use routini::{
    adaptive_loadbalancer::{
        AdaptiveLoadBalancer, decision_engine::AdaptiveDecisionEngine, options::AdaptiveLbOpt,
    },
    load_balancing::{Backends, Metrics, discovery::Static, strategy::Adaptive},
};
use tokio::sync::watch;

type Lb = AdaptiveLoadBalancer<AdaptiveDecisionEngine>;

fn start_lb(
    starting_strategy: Adaptive,
) -> (Arc<Lb>, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let discovery = Static::try_from_iter(["127.0.0.1:7001", "127.0.0.1:7002"]).unwrap();
    let opt = AdaptiveLbOpt {
        evaluate_strategy_frequency: Duration::from_millis(20),
        health_check_interval: None,
        starting_strategy,
        ..Default::default()
    };
    let engine = AdaptiveDecisionEngine::new(&opt);
    let lb = Arc::new(Lb::from_backends(
        Backends::new(discovery),
        Some(opt),
        engine,
    ));
    let (tx, rx) = watch::channel(false);
    let bg = lb.clone();
    let handle = tokio::spawn(async move { bg.start(rx).await });
    (lb, tx, handle)
}

async fn wait_for(lb: &Lb, expected: Adaptive) {
    for _ in 0..100 {
        if lb.current_strategy().await == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {expected}, still {}", lb.current_strategy().await);
}

async fn set_latencies(lb: &Lb, ms: [u64; 2]) {
    // The background service populates backends on its first update
    while lb.backends().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for (backend, ms) in lb.backends().iter().zip(ms) {
        // alpha 1.0 replaces the average outright
        backend
            .metrics
            .record_latency(Duration::from_millis(ms), 1.0);
    }
}

#[tokio::test]
async fn switches_to_fastest_server_on_latency_divergence_and_back_to_starting_strategy() {
    let (lb, tx, _handle) = start_lb(Adaptive::RoundRobin);
    wait_for(&lb, Adaptive::RoundRobin).await;

    set_latencies(&lb, [10, 100]).await;
    wait_for(&lb, Adaptive::FastestServer).await;
    let fastest = lb.backends().iter().next().unwrap().clone();
    assert_eq!(
        lb.select(b"").unwrap(),
        fastest,
        "should pick the low latency backend"
    );

    set_latencies(&lb, [50, 50]).await;
    wait_for(&lb, Adaptive::RoundRobin).await;
    tx.send(true).unwrap();
}

#[tokio::test]
async fn hash_strategy_keeps_affinity_under_latency_divergence() {
    let (lb, tx, _handle) = start_lb(Adaptive::Consistent);
    wait_for(&lb, Adaptive::Consistent).await;
    set_latencies(&lb, [10, 100]).await;
    // Several evaluation rounds (every 20ms)
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lb.current_strategy().await, Adaptive::Consistent);
    let first = lb.select(b"client-a").unwrap();
    assert_eq!(lb.select(b"client-a").unwrap(), first);
    tx.send(true).unwrap();
}

#[tokio::test]
async fn background_service_stops_promptly_on_shutdown() {
    let (_lb, tx, handle) = start_lb(Adaptive::RoundRobin);
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(500), handle)
        .await
        .expect("background service did not stop within 500ms")
        .unwrap();
}

#[tokio::test]
async fn manual_override_becomes_the_new_fallback() {
    let (lb, tx, _handle) = start_lb(Adaptive::RoundRobin);
    set_latencies(&lb, [50, 50]).await;
    wait_for(&lb, Adaptive::RoundRobin).await;

    assert!(lb.update_strategy(Adaptive::Random).await);
    // Several evaluation cycles pass, the engine must not revert the override
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lb.current_strategy().await, Adaptive::Random);

    set_latencies(&lb, [10, 100]).await;
    wait_for(&lb, Adaptive::FastestServer).await;
    set_latencies(&lb, [50, 50]).await;
    wait_for(&lb, Adaptive::Random).await;
    tx.send(true).unwrap();
}
