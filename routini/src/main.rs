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
        StrategyConfig::new("round_robin", StrategyKind::RoundRobin),
        StrategyConfig::new("random", StrategyKind::Random),
    ];

    let routing = RoutingConfig::new("round_robin");

    let app = Application::new(listener, backends, strategies, routing);
    app.run();
}
