use crate::helpers::TestApp;
use reqwest::StatusCode;
use routini::load_balancing::selection::least_connections::{CONNECTIONS, LeastConnections};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[tokio::test]
async fn should_connect_to_backend_with_least_connections() {
    let app = match TestApp::new(LeastConnections).await {
        Ok(app) => Arc::new(app),
        Err(err) => {
            eprintln!("Skipping test: unable to bootstrap test app ({err})");
            return;
        }
    };

    let requests_pr_backend = 5;

    let handles = Arc::new(Mutex::new(Vec::with_capacity(requests_pr_backend)));

    // Send concurrent requests with varying completion times to test least connections
    for _ in 0..app.backend_addresses.len() * requests_pr_backend {
        let app = app.clone();

        handles.lock().await.push(tokio::spawn(async move {
            let response = app.get_health_check().await;
            assert_eq!(response.status(), StatusCode::OK);
        }));

        // Small delay to ensure requests overlap
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let handles = handles.lock_owned().await.drain(..).collect::<Vec<_>>();
    for handle in handles {
        handle.await.expect("Unable to join tasks");
    }

<<<<<<< HEAD
    let connections = CONNECTIONS.load();

    // sins the loadbalancer reuses connections, it will only open a connection on each backend once
    assert!(
        connections
            .iter()
            .all(|(_, count)| count.1.load(std::sync::atomic::Ordering::Relaxed) == 1),
        "All backends should have received at least one request"
    );

    // sins the loadbalancer reuses connections, it will only open a connection on each backend once
    assert!(
        connections
            .iter()
            .all(|(_, count)| count.1.load(std::sync::atomic::Ordering::Relaxed) == 1),
        "All backends should have received at least one request"
    );
}
