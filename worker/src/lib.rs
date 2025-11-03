use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures::stream::{self, Stream};
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

async fn keep_alive(
    State(address): State<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    println!("Keep-alive connection opened on {}", address);

    let stream = stream::unfold(0u64, move |count| async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let event = Event::default()
            .data(format!("ping {}", count))
            .event("keepalive");
        Some((Ok(event), count + 1))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub struct Worker;

impl Worker {
    pub async fn run(listener: TcpListener) {
        let address = listener.local_addr().unwrap().to_string();

        let app = Router::new()
            .route("/", get(test_page))
            .route("/health", get(health))
            .route("/work", get(work))
            .route("/keep-alive", get(keep_alive))
            .with_state(address.clone());

        println!("Worker listening on {}", address);
        axum::serve(listener, app).await.unwrap();
    }
}
