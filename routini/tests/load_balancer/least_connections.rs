use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use routini::least_connections::LeastConnections;
use tokio::sync::Mutex;

use crate::helpers::TestApp;

#[tokio::test]
async fn should_connect_to_backend_with_least_connections() {
    let app = Arc::new(TestApp::<LeastConnections>::new().await);
    let total_requests = 100;
    let connections = Arc::new(Mutex::new(vec![0; app.backend_addresses.len()]));
    let active_connections = Arc::new(Mutex::new(vec![0; app.backend_addresses.len()]));

    let handles = Arc::new(Mutex::new(Vec::with_capacity(total_requests)));

    // Send concurrent requests with varying completion times to test least connections
    for i in 0..total_requests {
        let app = app.clone();
        let connections = connections.clone();
        let active_connections = active_connections.clone();

        handles.lock().await.push(tokio::spawn(async move {
            let response = app.get_health_check().await;
            assert_eq!(response.status(), StatusCode::OK);
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
        }));

        // Small delay to ensure requests overlap
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let handles = handles.lock_owned().await.drain(..).collect::<Vec<_>>();
    for handle in handles {
        handle.await.expect("Unable to join tasks");
    }

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
}
