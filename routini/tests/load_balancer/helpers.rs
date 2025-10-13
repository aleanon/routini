use std::marker::PhantomData;

use pingora::lb::selection::{BackendIter, BackendSelection};
use routini::application::Application;
use tokio::net::TcpListener;

pub struct TestApp<A> {
    pub server_address: String,
    pub backend_addresses: Vec<String>,
    pub http_client: reqwest::Client,
    _selection_algorithm: PhantomData<A>,
}

impl<A> TestApp<A>
where
    A: BackendSelection + 'static + Send + Sync,
    A::Iter: BackendIter,
{
    pub async fn new() -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to establish listener");
        let server_address = format!("http://{}", listener.local_addr().unwrap());

        let backend_listeners = vec![
            TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Failed to establish backend listener"),
            TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Failed to establish backend listener"),
            TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Failed to establish backend listener"),
        ];

        let backend_addresses = backend_listeners
            .iter()
            .map(|l| l.local_addr().unwrap().to_string())
            .collect::<Vec<_>>();

        tokio::spawn(async move {
            workers::Workers::run(backend_listeners).await;
        });

        let backend_addr_clone = backend_addresses.clone();
        std::thread::spawn(move || {
            Application::<A>::new(listener, backend_addr_clone).run();
        });

        let http_client = reqwest::Client::builder()
            .build()
            .expect("Failed to build http client");

        Self {
            server_address,
            backend_addresses,
            http_client,
            _selection_algorithm: PhantomData,
        }
    }

    pub async fn get_health_check(&self) -> reqwest::Response {
        self.http_client
            .get(format!("{}{}", self.server_address, "/health"))
            .send()
            .await
            .expect("Failed to send health check request")
    }
}
