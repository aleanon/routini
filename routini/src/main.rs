use routini::{
    load_balancing::strategy::Adaptive,
    server_builder::{Route, RouteConfig, proxy_server},
    utils::{
        constants::{DEFAULT_MAX_ALGORITHM_ITERATIONS, SET_STRATEGY_ENDPOINT_ADDRESS},
        tracing::init_tracing,
    },
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
    let route = Route::new(
        "/auth/{*rest}",
        backends,
        Adaptive::RoundRobin,
        true,
        DEFAULT_MAX_ALGORITHM_ITERATIONS,
        RouteConfig {
            strip_path_prefix: true,
        },
    );

    proxy_server(listener)
        .add_route(route)
        .set_strategy_endpoint(SET_STRATEGY_ENDPOINT_ADDRESS.to_string())
        .build()
        .run_forever();
}
