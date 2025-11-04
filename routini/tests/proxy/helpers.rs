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

        let app = Self {
            server_address,
            http_client,
        };

        // Wait for server to be ready by trying to connect
        for attempt in 0..50 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            // Try a basic connection to see if server is up
            if app
                .http_client
                .get(&app.server_address)
                .send()
                .await
                .is_ok()
            {
                return Ok(app);
            }
            if attempt == 49 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Server did not become ready in time",
                ));
            }
        }

        Ok(app)
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

        // Wait for backends to be ready
        let client = reqwest::Client::new();
        for backend_addr in &backend_addresses {
            for attempt in 0..50 {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                if client
                    .get(format!("http://{}/health", backend_addr))
                    .send()
                    .await
                    .is_ok()
                {
                    break;
                }
                if attempt == 49 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("Backend {} did not become ready in time", backend_addr),
                    ));
                }
            }
        }

        Ok(backend_addresses)
    }
}
