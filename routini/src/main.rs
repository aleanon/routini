use routini::{
    application::{Application, StrategyConfig, StrategyKind},
    load_balancer::RoutingConfig,
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

    let strategies = vec![
        StrategyConfig::new(StrategyKind::RoundRobin),
        StrategyConfig::new(StrategyKind::Random),
        StrategyConfig::new(StrategyKind::LeastConnections),
    ];

    let routing = RoutingConfig::new(StrategyKind::RoundRobin);

    let app = Application::new(listener, backends, strategies, routing);
    app.run();
}
