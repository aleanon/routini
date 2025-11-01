use color_eyre::eyre::Result;
use routini::{
    load_balancing::strategy::Adaptive,
    server_builder::{Route, proxy_server},
    utils::{constants::SET_STRATEGY_ENDPOINT_ADDRESS, tracing::init_tracing},
};
use std::net::TcpListener;

fn main() -> Result<()> {
    init_tracing().expect("Failed to set up tracing");
    color_eyre::install().expect("Failed to install color_eyre");

    let listener = TcpListener::bind("127.0.0.1:3500")?;
    let mut backends = Vec::new();

    for i in 1..=40 {
        backends.push(format!("127.0.0.1:40{:02}", i));
    }

    let route_two = Route::new("/api/*", backends, Adaptive::RoundRobin)?;

    proxy_server(listener)
        .add_route(route_two)
        .set_strategy_endpoint(SET_STRATEGY_ENDPOINT_ADDRESS.to_string())
        .build()
        .run_forever();
}
