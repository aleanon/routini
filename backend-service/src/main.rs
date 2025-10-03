use axum::{Router, extract::State, response::Html, routing::get};

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

#[tokio::main]
async fn main() {
    let mut listeners = Vec::new();
    for i in 0..3 {
        let address = format!("127.0.0.1:400{}", i);
        let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
        listeners.push((address, listener));
    }

    let mut servers = Vec::new();
    for (addr, listener) in listeners {
        let app = Router::new()
            .route("/", get(test_page))
            .with_state(addr.clone());

        let server = tokio::task::spawn(async move {
            println!("Server listening on {}", addr);
            axum::serve(listener, app).await.unwrap();
        });

        servers.push(server);
    }

    for server in servers {
        server.await.unwrap();
    }
}
