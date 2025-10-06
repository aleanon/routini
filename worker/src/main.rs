use worker::Workers;

#[tokio::main]
async fn main() {
    let listeners = [
        tokio::net::TcpListener::bind("127.0.0.1:4000")
            .await
            .unwrap(),
        tokio::net::TcpListener::bind("127.0.0.1:4001")
            .await
            .unwrap(),
        tokio::net::TcpListener::bind("127.0.0.1:4002")
            .await
            .unwrap(),
    ];

    Workers::run(listeners).await;
}
