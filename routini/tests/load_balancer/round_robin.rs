use crate::helpers::TestApp;
use reqwest::StatusCode;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn should_distribute_requests_evenly_among_backends() {
    let app = match TestApp::new().await {
        Ok(app) => app,
        Err(err) => {
            eprintln!("Skipping test: unable to bootstrap test app ({err})");
            return;
        }
    };

    let connections_pr_worker: usize = 200;
    let mut connections = vec![0; app.backend_addresses.len()];

    for _ in 0..app.backend_addresses.len() * connections_pr_worker {
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
    assert!(
        connections
            .iter()
            .all(|count| *count == connections_pr_worker)
    );
}

#[tokio::test]
async fn should_distribute_requests_evenly_with_concurrent_requests() {
    let app = match TestApp::new().await {
        Ok(app) => Arc::new(app),
        Err(err) => {
            eprintln!("Skipping test: unable to bootstrap test app ({err})");
            return;
        }
    };

    let connections_pr_worker: usize = 1000;
    let connections = Arc::new(Mutex::new(vec![0; app.backend_addresses.len()]));

    let handles = Arc::new(Mutex::new(Vec::with_capacity(app.backend_addresses.len())));

    for _ in 0..app.backend_addresses.len() * connections_pr_worker {
        let app = app.clone();
        let connections = connections.clone();
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

            connections.lock().await[index] += 1;
        }));
    }

    let handles = handles.lock_owned().await.drain(..).collect::<Vec<_>>();
    for handle in handles {
        handle.await.expect("Unable to join tasks")
    }

    assert!(
        connections
            .lock()
            .await
            .iter()
            .all(|count| *count == connections_pr_worker)
    );
}
