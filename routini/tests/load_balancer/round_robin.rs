use crate::helpers::TestApp;
use reqwest::StatusCode;

#[tokio::test]
async fn should_distribute_requests_evenly_among_backends() {
    let app = TestApp::new().await;
    let iterations: usize = 20;
    let mut connections = vec![0; app.backend_addresses.len()];

    for _ in 0..app.backend_addresses.len() * iterations {
        let response = app.get_health_check().await;
        assert_eq!(response.status(), StatusCode::OK);
        let backend_address = response.text().await.expect("Failed to parse response");

        let index = app
            .backend_addresses
            .iter()
            .enumerate()
            .find_map(|(i, s)| if s == &backend_address { Some(i) } else { None })
            .unwrap();

        connections[index] += 1;
    }
    assert!(connections.iter().all(|count| *count == iterations));
}
