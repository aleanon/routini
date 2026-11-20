use color_eyre::eyre::{Result, eyre};
use routini::{
    shutdown,
    utils::{
        config_loader::{config_path, load_config_from},
        tracing::{LogConfig, init_tracing_with_config},
    },
};
use std::path::PathBuf;

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

    config
        .into_server_builder()?
        .reload_on_sighup(config_path)
        .build()
        .run(shutdown::run_args());

    tracing::info!("Shutdown complete");
    Ok(())
}
