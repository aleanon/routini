use color_eyre::eyre::Result;
use routini::{
    load_balancing::strategy::Adaptive,
    server_builder::{Route, proxy_server},
    utils::{
        constants::{
            DEFAULT_LOG_JSON, DEFAULT_LOG_LEVEL_FILTER, DEFAULT_MAX_LOG_AGE_DAYS,
            SET_STRATEGY_ENDPOINT_ADDRESS,
        },
        tracing::{LogConfig, init_tracing_with_config},
    },
};
use std::net::TcpListener;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() -> Result<()> {
    // Configure logging based on environment
    let log_config = LogConfig {
        filter: std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_LEVEL_FILTER.to_string()),

        log_dir: std::env::var("LOG_DIR").ok(),

        file_prefix: "routini".to_string(),

        json_format: std::env::var("LOG_JSON")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(DEFAULT_LOG_JSON),

        ansi: std::env::var("NO_COLOR").is_err(),

        max_log_age_days: std::env::var("MAX_LOG_AGE_DAYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_LOG_AGE_DAYS),
    };

    init_tracing_with_config(log_config).expect("Failed to set up tracing");
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
