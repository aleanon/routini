use color_eyre::eyre::{Result, WrapErr, eyre};
use daemonize::{Daemonize, Stdio};
use routini::{
    shutdown,
    utils::{
        config_loader::{config_path, load_config_from},
        runtime_paths::check_private_parent,
        tracing::{LogConfig, init_tracing_with_config},
    },
};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
Usage: routini [--config <path>] [--check] [--upgrade] [--daemon]

Options:
  -c, --config <path>  Config file, JSON or TOML (default: $ROUTINI_CONFIG, then
                       ./config.toml or ./config.json)
      --check          Validate the config file and exit
  -u, --upgrade        Take over the listening sockets of a running routini (graceful
                       upgrade). Start the new process with this, then send SIGQUIT to the
                       old one
  -d, --daemon         Run in the background, writing server.pid_file
  -h, --help           Print this help

Signals:
  SIGHUP               Reload the config file in place
  SIGTERM / SIGINT     Graceful shutdown, a second one forces exit
  SIGQUIT              Hand listening sockets to a process started with --upgrade, then exit";

#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    check: bool,
    upgrade: bool,
    daemon: bool,
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
            "-u" | "--upgrade" => args.upgrade = true,
            "-d" | "--daemon" => args.daemon = true,
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

/// Fork into the background. Done here rather than by pingora (which forks inside
/// `Server::run`) because that runs after tracing has started its file writer thread, which
/// does not survive a fork. Unlike pingora this keeps the working directory, so relative paths
/// in the config and `.env` keep working.
fn daemonize(pid_file: &str) -> Result<()> {
    // In a directory only we can write to, the rename-then-create below can't be raced into
    // following an attacker's symlink
    check_private_parent(Path::new(pid_file)).wrap_err("Refusing to write the PID file")?;
    // Keep the previous process's pid file during an upgrade, as pingora does
    let pid_path = Path::new(pid_file);
    if pid_path.exists() {
        let _ = std::fs::rename(pid_path, format!("{pid_file}.old"));
    }
    Daemonize::new()
        .pid_file(pid_file)
        .working_directory(std::env::current_dir()?)
        .umask(0o007)
        .stdout(Stdio::keep())
        .stderr(Stdio::keep())
        .start()
        .map_err(|err| eyre!("Failed to daemonize: {err}"))
}

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() -> Result<()> {
    // Load .env before anything reads the environment. Variables already set in the real
    // environment take precedence, the file only fills in what is missing.
    let dotenv = dotenvy::dotenv();
    color_eyre::install().expect("Failed to install color_eyre");

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

    let upgrade_sock = config.server.upgrade_sock();
    if args.upgrade {
        // The new process binds this socket and receives the listening sockets on it
        check_private_parent(Path::new(&upgrade_sock))
            .wrap_err("Refusing to use the upgrade socket")?;
    }
    // Before tracing, which may start a background writer thread
    if args.daemon {
        daemonize(&config.server.pid_file())?;
    }

    init_tracing_with_config(LogConfig::from_env()).expect("Failed to set up tracing");

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
    tracing::info!(
        config = %path.display(),
        upgrade = args.upgrade,
        "Loaded configuration: {} route(s)",
        config.proxy.router.len()
    );

    config
        .into_server_builder_with(args.upgrade)?
        .reload_on_sighup(path.to_string_lossy().into_owned())
        .build()
        .run(shutdown::run_args(upgrade_sock));

    tracing::info!("Shutdown complete");
    Ok(())
}
