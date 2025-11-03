use worker::Worker;

#[tokio::main]
async fn main() {
    // Get port from environment variable, default to 4000
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "4000".to_string())
        .parse::<u16>()
        .expect("PORT must be a valid number");

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|_| panic!("Failed to bind to {}", addr));

    println!("Starting worker on {}", addr);
    Worker::run(listener).await;
}
