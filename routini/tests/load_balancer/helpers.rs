use std::{io, net::Ipv4Addr};

use fake::{Fake, Faker};
use routini::{
    load_balancing::strategy::Adaptive,
    server_builder::{Route, RouteConfig, proxy_server},
};
use tokio::net::TcpListener;

pub struct TestApp {
    pub server_address: String,
    pub backend_addresses: Vec<String>,
    pub http_client: reqwest::Client,
}

impl TestApp {
    pub async fn new(selection_strategy: Adaptive) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let server_address = format!("http://{}", listener.local_addr()?);

        let mut backend_listeners = Vec::new();
        for _ in 0..3 {
            backend_listeners.push(TcpListener::bind("127.0.0.1:0").await?);
        }

        let backend_addresses = backend_listeners
            .iter()
            .map(|l| l.local_addr().map(|addr| addr.to_string()))
            .collect::<io::Result<Vec<_>>>()?;

        for listener in backend_listeners {
            tokio::spawn(async move {
                worker::Worker::run(listener).await;
            });
        }

        let backend_addr_clone = backend_addresses.clone();
        std::thread::spawn(move || {
            let route = Route::new("/health", backend_addr_clone, selection_strategy)
                .expect("Failed to construct route")
                .route_config(RouteConfig {
                    strip_path_prefix: false,
                });

            proxy_server(listener)
                .add_route(route)
                .build()
                .run_forever();
        });

        let http_client = reqwest::Client::builder()
            .build()
            .expect("Failed to build http client");

        let app = Self {
            server_address,
            backend_addresses,
            http_client,
        };

        // Wait for server to be ready by polling the health endpoint
        for attempt in 0..50 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if let Ok(response) = app
                .http_client
                .get(format!("{}/health", app.server_address))
                .send()
                .await
            {
                if response.status().is_success() {
                    return Ok(app);
                }
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

    pub async fn get_health_check(&self) -> reqwest::Response {
        let ip: Ipv4Addr = Faker.fake();

        self.http_client
            .get(format!("{}{}", self.server_address, "/health"))
            .header("x-forwarded-for", ip.to_string())
            .send()
            .await
            .expect("Failed to send health check request")
    }
}
