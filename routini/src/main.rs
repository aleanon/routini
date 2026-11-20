use color_eyre::eyre::{Result, eyre};
use pingora::server::configuration::ServerConf;
use routini::{
    server_builder::proxy_server,
    shutdown,
    utils::{
        config_loader::{CONFIG_PATH_ENV, DEFAULT_CONFIG_PATH, load_config_from},
        tracing::{LogConfig, init_tracing_with_config},
    },
};
use std::net::TcpListener;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() -> Result<()> {
    // Load .env before anything reads the environment. Variables already set in the real
    // environment take precedence, the file only fills in what is missing.
    let dotenv = dotenvy::dotenv();

    init_tracing_with_config(LogConfig::from_env()).expect("Failed to set up tracing");
    color_eyre::install().expect("Failed to install color_eyre");

    match dotenv {
        Ok(path) => tracing::info!(path = %path.display(), "Loaded environment file"),
        Err(err) if err.not_found() => {}
        Err(err) => tracing::warn!("Failed to load .env file: {err}"),
    }

    let config_path =
        std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let config = load_config_from(&config_path)?;
    tracing::info!(
        "Loaded configuration: {} route(s)",
        config.proxy.router.len()
    );

    let listener = TcpListener::bind(config.listen_address())?;

    let routes = config.routes()?;
    if routes.is_empty() {
        return Err(eyre!("Configuration must define at least one route"));
    }

    let mut builder = proxy_server(listener);

    if config.server.has_runtime_tuning() {
        let mut conf = ServerConf::default();
        conf.upstream_keepalive_pool_size =
            config.server.upstream_keepalive_pool_size.unwrap_or(200000);
        if let Some(threads) = config.server.worker_threads {
            conf.threads = threads;
        }
        conf.grace_period_seconds = config.server.grace_period_seconds;
        conf.graceful_shutdown_timeout_seconds = config.server.graceful_shutdown_timeout_seconds;
        builder = builder.server_config(conf);
    }

    for route in routes {
        builder = builder.add_route(route);
    }

    if let Some(prometheus_address) = config.server.prometheus_address.clone() {
        builder = builder.prometheus_address(prometheus_address);
    }

    if let Some(access_log) = config.server.access_log {
        builder = builder.access_log(access_log);
    }

    if let Some(https_redirect) = config.server.https_redirect {
        builder = builder.https_redirect(https_redirect);
    }

    if let Some(level) = config.server.compression_level {
        builder = builder.compression_level(level);
    }

    if let Some(request_id) = config.server.request_id {
        builder = builder.request_id(request_id);
    }

    if !config.server.error_pages.is_empty() {
        builder = builder.error_pages(config.server.error_pages.clone());
    }

    if let Some(tls) = &config.proxy.tls {
        builder = builder.tls(tls.to_builder_tls());
    }

    builder = builder.reload_on_sighup(config_path);

    builder.build().run(shutdown::run_args());

    tracing::info!("Shutdown complete");
    Ok(())
}
