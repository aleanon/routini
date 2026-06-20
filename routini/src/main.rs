use color_eyre::eyre::{Result, eyre};
use routini::{
    server_builder::proxy_server,
    utils::{
        config_loader::load_config,
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

    let config = load_config()?;
    tracing::info!("Loaded configuration: {} route(s)", config.proxy.router.len());

    let listener = TcpListener::bind(config.listen_address())?;

    let routes = config.routes()?;
    if routes.is_empty() {
        return Err(eyre!("Configuration must define at least one route"));
    }

    let mut builder = proxy_server(listener);
    for route in routes {
        builder = builder.add_route(route);
    }

    let strategy_endpoint = config
        .server
        .set_strategy_endpoint
        .clone()
        .unwrap_or_else(|| SET_STRATEGY_ENDPOINT_ADDRESS.to_string());
    builder = builder.set_strategy_endpoint(strategy_endpoint);

    if let Some(prometheus_address) = config.server.prometheus_address.clone() {
        builder = builder.prometheus_address(prometheus_address);
    }

    if let Some(access_log) = config.server.access_log {
        builder = builder.access_log(access_log);
    }

    if let Some(https_redirect) = config.server.https_redirect {
        builder = builder.https_redirect(https_redirect);
    }

    if let Some(tls) = &config.proxy.tls {
        builder = builder.tls(tls.to_builder_tls());
    }

    builder.build().run_forever();
}
