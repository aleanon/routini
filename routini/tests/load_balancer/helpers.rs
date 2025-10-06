use routini::application::Application;
use tokio::net::TcpListener;

pub struct TestApp {
    pub server_address: String,
    pub backend_addresses: Vec<String>,
    pub http_client: reqwest::Client,
}

impl TestApp {
    pub async fn new() -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to establish listener");
        let port = listener.local_addr().unwrap().port();
        let server_address = format!("http://127.0.0.1:{}", port);

        // Creates the listeners so the operating system can assign random available ports
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

        //Take ownership of the listeners so they are dropped here, to free the ports for the application
        let backend_addresses = backend_listeners
            .iter()
            .map(|l| l.local_addr().unwrap().to_string())
            .collect::<Vec<_>>();

        tokio::spawn(async move {
            worker::Workers::run(backend_listeners).await;
        });

        let backend_addr_clone = backend_addresses.clone();
        std::thread::spawn(move || {
            Application::new(listener, backend_addr_clone).run();
        });

        let http_client = reqwest::Client::builder()
            .build()
            .expect("Failed to build http client");

        Self {
            server_address,
            backend_addresses,
            http_client,
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
