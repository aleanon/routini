//! Signal handling for graceful shutdown.
//!
//! Pingora's default watcher treats SIGINT as a *fast* shutdown that drops in-flight requests.
//! [RoutiniShutdownSignal] makes SIGINT graceful as well, so Ctrl+C and `docker stop` (SIGTERM)
//! both drain:
//!
//! | Signal              | Behaviour                                                         |
//! |---------------------|-------------------------------------------------------------------|
//! | SIGTERM             | graceful: stop accepting, wait the grace period, stop runtimes    |
//! | SIGINT (first)      | graceful, same as SIGTERM                                         |
//! | SIGINT/SIGTERM again| force exit immediately with code 130, for when draining hangs     |
//! | SIGQUIT             | graceful upgrade (hands listening sockets to a new process), only  |
//! |                     | if the upgrade socket is ours, otherwise logged and ignored       |
//!
//! The grace period and runtime shutdown timeout are configured with
//! [crate::server_builder::ServerBuilder::grace_period] and
//! [crate::server_builder::ServerBuilder::graceful_shutdown_timeout].

use std::path::PathBuf;

use async_trait::async_trait;
use pingora::server::{RunArgs, ShutdownSignal, ShutdownSignalWatch};
use tokio::signal::unix::{SignalKind, signal};

use crate::utils::runtime_paths::check_upgrade_socket;

/// Exit code used when a second signal forces the process down (128 + SIGINT).
const FORCED_EXIT_CODE: i32 = 130;

pub struct RoutiniShutdownSignal {
    /// Checked on SIGQUIT before pingora hands the listening sockets to whoever listens on it.
    pub upgrade_sock: PathBuf,
}

#[async_trait]
impl ShutdownSignalWatch for RoutiniShutdownSignal {
    async fn recv(&self) -> ShutdownSignal {
        let mut upgrade = signal(SignalKind::quit()).expect("Failed to install SIGQUIT handler");
        let mut terminate =
            signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
        let mut interrupt =
            signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");

        let shutdown = loop {
            tokio::select! {
                _ = upgrade.recv() => match check_upgrade_socket(&self.upgrade_sock) {
                    Ok(()) => return ShutdownSignal::GracefulUpgrade,
                    Err(err) => tracing::error!(
                        "SIGQUIT received, refusing graceful upgrade and keeping this process \
                         running: {err:#}"
                    ),
                },
                _ = terminate.recv() => break "SIGTERM",
                _ = interrupt.recv() => break "SIGINT",
            }
        };

        tracing::info!(
            signal = shutdown,
            "Graceful shutdown started, no longer accepting new connections. \
             Send SIGINT or SIGTERM again to force exit"
        );

        // The installed handlers replace the default disposition for the lifetime of the
        // process, so without this a second Ctrl+C would be ignored while draining. The server
        // runtime keeps running spawned tasks after `recv` returns.
        tokio::spawn(async move {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            tracing::warn!("Second shutdown signal received, forcing exit");
            std::process::exit(FORCED_EXIT_CODE);
        });

        ShutdownSignal::GracefulTerminate
    }
}

/// [RunArgs] using [RoutiniShutdownSignal]. Pass to [pingora::server::Server::run].
/// `upgrade_sock` must match the server's [crate::config::ProcessSection::upgrade_sock].
pub fn run_args(upgrade_sock: impl Into<PathBuf>) -> RunArgs {
    RunArgs {
        shutdown_signal: Box::new(RoutiniShutdownSignal {
            upgrade_sock: upgrade_sock.into(),
        }),
    }
}
