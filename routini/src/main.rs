use color_eyre::eyre::{Result, eyre};
use pingora::server::configuration::ServerConf;
use routini::{
    server_builder::proxy_server,
    shutdown,
    utils::{
        config_loader::{config_path, load_config_from},
        tracing::{LogConfig, init_tracing_with_config},
    },
};
use std::{net::TcpListener, path::PathBuf};

const USAGE: &str = "\
Usage: routini [--config <path>] [--check]

Options:
  -c, --config <path>  Config file, JSON or TOML (default: $ROUTINI_CONFIG, then
                       ./config.toml or ./config.json)
      --check          Validate the config file and exit
  -h, --help           Print this help

Signals:
  SIGHUP               Reload the config file
  SIGTERM / SIGINT     Graceful shutdown, a second one forces exit";

#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    check: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-c" | "--config" => match iter.next() {
                Some(path) => args.config = Some(path.into()),
                None => return Err(eyre!("{arg} requires a path\n\n{USAGE}")),
            },
            "--check" => args.check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => match other.strip_prefix("--config=") {
                Some(path) => args.config = Some(path.into()),
                None => return Err(eyre!("Unknown argument '{other}'\n\n{USAGE}")),
            },
        }
    }
    Ok(args)
}

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() -> Result<()> {
    // Load .env before anything reads the environment. Variables already set in the real
    // environment take precedence, the file only fills in what is missing.
    let dotenv = dotenvy::dotenv();

    init_tracing_with_config(LogConfig::from_env()).expect("Failed to set up tracing");
    color_eyre::install().expect("Failed to install color_eyre");

    // Never log the error text: a parse error contains the offending line, and .env lines are
    // often secrets
    match dotenv {
        Ok(path) => tracing::info!(path = %path.display(), "Loaded environment file"),
        Err(err) if err.not_found() => {}
        Err(dotenvy::Error::LineParse(_, index)) => tracing::warn!(
            position = index,
            "Stopped loading .env at a line with invalid syntax, later lines are not applied (content not logged)"
        ),
        Err(dotenvy::Error::Io(err)) => {
            tracing::warn!(kind = ?err.kind(), "Ignoring .env file: it could not be read")
        }
        Err(_) => tracing::warn!("Ignoring .env file: it could not be loaded"),
    }

    let args = parse_args()?;
    // Absolute, so SIGHUP reloads the same file whatever the working directory is by then
    let path = config_path(args.config)?;
    let path = path.canonicalize().unwrap_or(path);
    let config = load_config_from(&path)?;

    if args.check {
        println!(
            "{}: OK ({} route(s))",
            path.display(),
            config.proxy.router.len()
        );
        return Ok(());
    }
    tracing::info!(
        config = %path.display(),
        "Loaded configuration: {} route(s)",
        config.proxy.router.len()
    );
    let config_path = path.to_string_lossy().into_owned();

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
