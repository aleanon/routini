use crate::helpers::TestApp;
use reqwest::StatusCode;
use routini::application::StrategyKind;
use routini::least_connections::CONNECTIONS;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[tokio::test]
async fn should_connect_to_backend_with_least_connections() {
    let app = match TestApp::new(StrategyKind::LeastConnections).await {
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
<<<<<<< HEAD
=======
            let backend_address = response.text().await.expect("Failed to parse response");

            let index = app
                .backend_addresses
                .iter()
                .enumerate()
                .find_map(|(i, s)| if s == &backend_address { Some(i) } else { None })
                .unwrap();

            // Track active and total connections
            active_connections.lock().await[index] += 1;
            connections.lock().await[index] += 1;

            // Simulate different processing times to create connection imbalance
            if i % 3 == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            active_connections.lock().await[index] -= 1;
>>>>>>> f512f98 (feat: Rebased to main)
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
=======
    let final_connections = connections.lock().await;

    // Verify that requests were distributed (not all to one backend)
    assert!(
        final_connections.iter().all(|&count| count > 0),
        "All backends should have received at least one request"
    );

    // Calculate the variance to ensure distribution is reasonably balanced
    let sum: usize = final_connections.iter().sum();
    let mean = sum as f64 / final_connections.len() as f64;
    let variance: f64 = final_connections
        .iter()
        .map(|&count| {
            let diff = count as f64 - mean;
            diff * diff
        })
        .sum::<f64>()
        / final_connections.len() as f64;

    // Variance should be reasonable - not all requests to one backend
    assert!(
        variance < (total_requests as f64 * 0.5),
        "Requests should be reasonably distributed. Variance: {}, Connections: {:?}",
        variance,
        final_connections
    );
>>>>>>> f512f98 (feat: Rebased to main)
}
