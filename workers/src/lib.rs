use std::time::Duration;

use axum::{Router, extract::State, http::StatusCode, response::Html, routing::get};
use tokio::net::TcpListener;

async fn test_page(State(address): State<String>) -> Html<&'static str> {
    println!("GET request received on {}", address);
    Html(
        r#"
        <!DOCTYPE html>
        <html>
            <head>
                <title>Test Page</title>
            </head>
            <body>
                <h1>Hello from Axum!</h1>
                <p>This is a small test page served by the backend service.</p>
            </body>
        </html>
        "#,
    )
}

async fn health(State(address): State<String>) -> (StatusCode, String) {
    println!("Health check sent from {}", &address);
    (StatusCode::OK, address)
}

async fn work(State(address): State<String>) -> StatusCode {
    println!("Work done on {}", address);
    tokio::time::sleep(Duration::from_millis(10)).await;
    StatusCode::OK
}

pub struct Workers;

impl Workers {
    pub async fn run(address_listeners: impl IntoIterator<Item = TcpListener>) {
        let mut workers = Vec::new();

        for listener in address_listeners {
            let address = listener.local_addr().unwrap().to_string();

            let app = Router::new()
                .route("/", get(test_page))
                .route("/health", get(health))
                .route("/work", get(work))
                .with_state(address.clone());

            let worker = tokio::task::spawn(async move {
                println!("Worker listening on {}", address);
                axum::serve(listener, app).await.unwrap();
            });

            workers.push(worker);
        }

        for worker in workers {
            worker.await.unwrap();
        }
    }
}
