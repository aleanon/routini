use routini::{
    application::Application, load_balancing::selection::adaptive::Adaptive,
    utils::tracing::init_tracing,
};
use std::net::TcpListener;

fn main() {
    init_tracing().expect("Failed to initialize tracing");

    let listener = TcpListener::bind("127.0.0.1:3500").expect("Failed to bind to address");
    let backends = [
        "127.0.0.1:4000".to_owned(),
        "127.0.0.1:4001".to_owned(),
        "127.0.0.1:4002".to_owned(),
    ];

    let app = Application::new(listener, backends, Adaptive::Consistent);
    app.run();
}
