use std::{io, net::Ipv4Addr};

use fake::{Fake, Faker};
use routini::{
    application::{Application, StrategyConfig, StrategyKind},
    proxy::RoutingConfig,
};
use tokio::net::TcpListener;

pub struct TestApp {
    pub server_address: String,
    pub backend_addresses: Vec<String>,
    pub http_client: reqwest::Client,
}

impl TestApp {
    pub async fn new(selection_strategy: StrategyKind) -> io::Result<Self> {
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

        tokio::spawn(async move {
            workers::Workers::run(backend_listeners).await;
        });

        let backend_addr_clone = backend_addresses.clone();
        std::thread::spawn(move || {
            let strategies = vec![
                StrategyConfig::new(StrategyKind::RoundRobin),
                StrategyConfig::new(StrategyKind::Random),
                StrategyConfig::new(StrategyKind::LeastConnections),
            ];
            let routing = RoutingConfig::new(selection_strategy);
            Application::new(listener, backend_addr_clone, strategies, routing).run();
        });

        let http_client = reqwest::Client::builder()
            .build()
            .expect("Failed to build http client");

        Ok(Self {
            server_address,
            backend_addresses,
            http_client,
        })
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
