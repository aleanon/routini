use reqwest::StatusCode;
use routini::{
    load_balancing::strategy::Adaptive,
    server_builder::{Route, RouteConfig},
};

use super::helpers::TestApp;

/// Tests that a request to a non-existent route returns NOT_FOUND
#[tokio::test]
async fn test_upstream_peer_not_found_route() {
    let backends = vec!["127.0.0.1:8001".to_string()];
    let route = Route::new("/api", backends, Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false)
        .route_config(RouteConfig {
            strip_path_prefix: false,
        });

    let app = TestApp::new(vec![route]).await.unwrap();

    let response = app
        .http_client
        .get(format!("{}/nonexistent", app.server_address))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Tests that routing works with actual backend servers
#[tokio::test]
async fn test_upstream_peer_with_real_backends() {
    let backend_addresses = TestApp::create_backends(2).await.unwrap();

    let route = Route::new("/health", backend_addresses, Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false)
        .route_config(RouteConfig {
            strip_path_prefix: false,
        });

    let app = TestApp::new(vec![route]).await.unwrap();

    let response = app
        .http_client
        .get(format!("{}/health", app.server_address))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), StatusCode::OK);
}

/// Tests that path stripping works correctly
#[tokio::test]
async fn test_upstream_peer_path_stripping() {
    let backend_addresses = TestApp::create_backends(1).await.unwrap();

    let route = Route::new("/api/*", backend_addresses, Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false);

    let app = TestApp::new(vec![route]).await.unwrap();

    // Request /api/health - should strip /api and send /health to backend
    let response = app
        .http_client
        .get(format!("{}/api/health", app.server_address))
        .send()
        .await
        .expect("Failed to send request");

    // Backend should receive request with path /health (stripped /api prefix)
    assert_eq!(response.status(), StatusCode::OK);
}

/// Tests multiple routes with different configurations
#[tokio::test]
async fn test_upstream_peer_multiple_routes() {
    let backend_addresses = TestApp::create_backends(2).await.unwrap();
    let addr1 = backend_addresses[0].clone();
    let addr2 = backend_addresses[1].clone();

    let route1 = Route::new("/work", vec![addr1], Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false);

    let route2 = Route::new("/api/*", vec![addr2], Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false);

    let app = TestApp::new(vec![route1, route2]).await.unwrap();

    let response1 = app
        .http_client
        .get(format!("{}/work", app.server_address))
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response1.status(), StatusCode::OK);

    let response2 = app
        .http_client
        .get(format!("{}/api/health", app.server_address))
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response2.status(), StatusCode::OK);
}

/// Tests load balancing across multiple backends
#[tokio::test]
async fn test_upstream_peer_load_balancing() {
    let backend_addresses = TestApp::create_backends(3).await.unwrap();

    let route = Route::new("/work", backend_addresses, Adaptive::default())
        .expect("Invalid route")
        .include_health_check(false)
        .route_config(RouteConfig {
            strip_path_prefix: false,
        });

    let app = TestApp::new(vec![route]).await.unwrap();

    // Make multiple requests to test load balancing
    for _ in 0..5 {
        let response = app
            .http_client
            .get(format!("{}/work", app.server_address))
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(response.status(), StatusCode::OK);
    }
}
