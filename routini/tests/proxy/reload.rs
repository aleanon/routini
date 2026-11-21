//! End-to-end config reload: a real server built from TOML, real backends, reloads applied
//! through the RouteManager handle and through SIGHUP.
use std::{collections::HashSet, sync::Arc, time::Duration};

use reqwest::StatusCode;
use routini::{
    load_balancing::{Metrics, strategy::Adaptive},
    utils::config_loader::load_config_from,
};

use crate::{
    config_app::{Ports, ReloadApp, route},
    helpers::TestApp,
};

#[tokio::test]
async fn reload_adds_and_removes_routes() {
    let b = TestApp::create_backends(2).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;

    assert_eq!(app.get("/a/health").await, (StatusCode::OK, b[0].clone()));
    assert_eq!(app.get("/b/health").await.0, StatusCode::NOT_FOUND);

    let report = app
        .manager
        .reload(ports.config(&route("/b/*", &[&b[1]], "", "")))
        .await
        .unwrap();
    assert_eq!(report.added, vec!["/b/*"]);
    assert_eq!(report.removed, vec!["/a/*"]);

    assert_eq!(app.get("/b/health").await, (StatusCode::OK, b[1].clone()));
    assert_eq!(app.get("/a/health").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reload_swaps_backends_and_keeps_metrics_of_retained_ones() {
    let b = TestApp::create_backends(2).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;
    assert_eq!(
        app.served_by("/a/health", 3).await,
        HashSet::from([b[0].clone()])
    );

    let lb = app.manager.load_balancer("/a/*").await.unwrap();
    let kept = lb.backends().first().unwrap().clone();
    kept.metrics.record_latency(Duration::from_millis(42), 1.0);

    let report = app
        .manager
        .reload(ports.config(&route("/a/*", &[&b[0], &b[1]], "", "")))
        .await
        .unwrap();
    assert_eq!(report.updated, vec!["/a/*"]);

    // Same load balancer, and the kept backend still carries its latency history
    let lb_after = app.manager.load_balancer("/a/*").await.unwrap();
    assert!(Arc::ptr_eq(&lb, &lb_after));
    let after = lb_after
        .backends()
        .iter()
        .find(|be| be.addr == kept.addr)
        .unwrap()
        .clone();
    assert_eq!(after.metrics.average_latency(), Some(42.0));

    assert_eq!(
        app.served_by("/a/health", 6).await,
        HashSet::from([b[0].clone(), b[1].clone()])
    );

    // Removing one stops traffic to it
    app.manager
        .reload(ports.config(&route("/a/*", &[&b[1]], "", "")))
        .await
        .unwrap();
    assert_eq!(
        app.served_by("/a/health", 4).await,
        HashSet::from([b[1].clone()])
    );
}

#[tokio::test]
async fn reload_applies_tuning_strategy_and_route_settings_live() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;
    let lb = app.manager.load_balancer("/a/*").await.unwrap();
    // Cache the stripped path for /a/health
    assert_eq!(app.get("/a/health").await.0, StatusCode::OK);

    // A manual override survives a reload that leaves the configured strategy alone
    assert!(lb.update_strategy(Adaptive::Random).await);
    let report = app
        .manager
        .reload(ports.config(&route("/a/*", &[&b[0]], "", "")))
        .await
        .unwrap();
    assert_eq!(report.unchanged, vec!["/a/*"]);
    assert_eq!(lb.current_strategy().await, Adaptive::Random);

    let changed = route(
        "/a/*",
        &[&b[0]],
        "strip_prefix = false",
        "strategy = \"Consistent\"\nmax_iterations = 7\n\
         [proxy.router.load_balancer.adaptive_lb_opt]\nmin_nr_of_connections = 3\n\
         evaluate_strategy_frequency_secs = 1\nhysteresis_exit_factor = 0.5",
    );
    let report = app.manager.reload(ports.config(&changed)).await.unwrap();
    assert_eq!(report.updated, vec!["/a/*"]);

    assert_eq!(lb.current_strategy().await, Adaptive::Consistent);
    assert_eq!(
        lb.decision_engine().default_strategy(),
        Adaptive::Consistent
    );
    assert_eq!(lb.config().max_iterations, 7);
    assert_eq!(
        lb.config().evaluate_strategy_frequency,
        Duration::from_secs(1)
    );
    let thresholds = lb.decision_engine().thresholds();
    assert_eq!(thresholds.min_nr_of_connections, 3);
    assert_eq!(thresholds.hysteresis_exit_factor, 0.5);
    // Prefix no longer stripped, even for the path cached before the reload: the worker gets
    // /a/health, which it doesn't serve
    assert_eq!(app.get("/a/health").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reload_adds_virtual_host_routes() {
    let b = TestApp::create_backends(2).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;

    let both = format!(
        "{}{}",
        route("/a/*", &[&b[0]], "", ""),
        route("/a/*", &[&b[1]], "host = \"Example.COM\"", "")
    );
    let report = app.manager.reload(ports.config(&both)).await.unwrap();
    assert_eq!(report.added, vec!["example.com/a/*"]);

    assert_eq!(app.get("/a/health").await.1, b[0]);
    assert_eq!(
        app.get_with_host("/a/health", Some("example.com")).await.1,
        b[1]
    );
    assert!(
        app.manager
            .load_balancer_for(Some("EXAMPLE.com"), "/a/*")
            .await
            .is_some()
    );
}

#[tokio::test]
async fn toggling_health_checks_rebuilds_the_route() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;
    let before = app.manager.load_balancer("/a/*").await.unwrap();

    let with_hc = route("/a/*", &[&b[0]], "", "").replace(
        "health_check_interval_secs = 0",
        "health_check_interval_secs = 1",
    );
    let report = app.manager.reload(ports.config(&with_hc)).await.unwrap();
    assert_eq!(report.rebuilt, vec!["/a/*"]);

    let after = app.manager.load_balancer("/a/*").await.unwrap();
    assert!(!Arc::ptr_eq(&before, &after));
    assert!(after.health_check_enabled());
    assert_eq!(app.get("/a/health").await, (StatusCode::OK, b[0].clone()));
}

#[tokio::test]
async fn invalid_config_is_rejected_and_server_settings_need_restart() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(ports.config(&route("/a/*", &[&b[0]], "", "")), None).await;

    let mut invalid = ports.config(&route("/a/*", &[&b[0]], "", ""));
    invalid.proxy.router[0].load_balancer.max_iterations = Some(0);
    assert!(app.manager.reload(invalid).await.is_err());
    assert_eq!(app.get("/a/health").await, (StatusCode::OK, b[0].clone()));

    let other = Ports::new();
    let report = app
        .manager
        .reload(other.config(&route("/a/*", &[&b[0]], "", "")))
        .await
        .unwrap();
    assert_eq!(report.restart_required, vec!["server", "proxy.listener"]);
    // Still serving on the original address
    assert_eq!(app.get("/a/health").await, (StatusCode::OK, b[0].clone()));
}

#[tokio::test]
async fn sighup_reloads_the_config_file() {
    let b = TestApp::create_backends(2).await.unwrap();
    let ports = Ports::new();
    let dir = std::env::temp_dir().join(format!("routini-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No .toml/.json extension: the format is detected from the content on every reload
    let file = dir.join("routini.conf");

    let write = |backend: &String| {
        std::fs::write(&file, ports.toml(&route("/a/*", &[backend], "", ""))).unwrap();
    };
    write(&b[0]);
    let app = ReloadApp::start(
        load_config_from(&file).unwrap(),
        Some(file.to_string_lossy().into_owned()),
    )
    .await;
    assert_eq!(app.get("/a/health").await, (StatusCode::OK, b[0].clone()));

    write(&b[1]);
    let status = std::process::Command::new("kill")
        .args(["-HUP", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let mut served = String::new();
    for _ in 0..50 {
        served = app.get("/a/health").await.1;
        if served == b[1] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(served, b[1]);
    std::fs::remove_dir_all(&dir).ok();
}
