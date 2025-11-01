use routini::server_builder::{Route, proxy_server};
use std::io;
use tokio::net::TcpListener;

pub struct TestApp {
    pub server_address: String,
    pub http_client: reqwest::Client,
}

impl TestApp {
    pub async fn new(routes: Vec<Route>) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let server_address = format!("http://{}", listener.local_addr()?);

        std::thread::spawn(move || {
            let mut server_builder = proxy_server(listener);
            for route in routes {
                server_builder = server_builder.add_route(route);
            }
            server_builder.build().run_forever();
        });

        let http_client = reqwest::Client::builder()
            .build()
            .expect("Failed to build http client");

        // Give the server a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        Ok(Self {
            server_address,
            http_client,
        })
    }

    /// Creates backend servers and returns their addresses
    pub async fn create_backends(num_backends: usize) -> io::Result<Vec<String>> {
        let mut backend_listeners = Vec::new();
        for _ in 0..num_backends {
            backend_listeners.push(TcpListener::bind("127.0.0.1:0").await?);
        }

        let backend_addresses: Vec<String> = backend_listeners
            .iter()
            .map(|l| l.local_addr().map(|addr| addr.to_string()))
            .collect::<io::Result<Vec<_>>>()?;

        for listener in backend_listeners {
            tokio::spawn(async move {
                worker::Worker::run(listener).await;
            });
        }

        // Give backends time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        Ok(backend_addresses)
    }
}
