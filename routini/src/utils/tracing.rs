use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tracing_error::ErrorLayer;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::utils::constants::{
    DEFAULT_LOG_JSON, DEFAULT_LOG_LEVEL_FILTER, DEFAULT_MAX_LOG_AGE_DAYS,
};

pub struct LogConfig {
    /// Log level filter (e.g., "info", "debug", "routini=debug,pingora=info")
    pub filter: String,
    /// Directory for log files (None = stdout only)
    pub log_dir: Option<String>,
    /// Log file name prefix (e.g., "routini" -> "routini.log")
    pub file_prefix: String,
    /// Enable JSON formatted logs instead of human-readable
    pub json_format: bool,
    /// Enable ANSI colors (only for non-JSON stdout)
    pub ansi: bool,
    /// Maximum age of log files in days (0 = keep forever)
    pub max_log_age_days: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: std::env::var("RUST_LOG")
                .unwrap_or_else(|_| DEFAULT_LOG_LEVEL_FILTER.to_string()),
            log_dir: None,
            file_prefix: "routini".to_string(),
            json_format: DEFAULT_LOG_JSON,
            ansi: true,
            max_log_age_days: std::env::var("LOG_MAX_AGE_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MAX_LOG_AGE_DAYS),
        }
    }
}

pub fn init_tracing() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing_with_config(LogConfig::default())
}

fn cleanup_old_logs(log_dir: &str, file_prefix: &str, max_age_days: u64) -> std::io::Result<()> {
    if max_age_days == 0 {
        return Ok(());
    }

    let log_path = PathBuf::from(log_dir);
    if !log_path.exists() {
        return Ok(());
    }

    let now = SystemTime::now();
    let max_age = Duration::from_secs(max_age_days * 24 * 60 * 60);

    tracing::debug!(
        log_dir = %log_dir,
        max_age_days = max_age_days,
        "Cleaning up old log files"
    );

    let mut deleted_count = 0;
    let mut total_size: u64 = 0;

    for entry in fs::read_dir(&log_path)? {
        let entry = entry?;
        let path = entry.path();

        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if !filename.starts_with(file_prefix) || !filename.contains(".log") {
            continue;
        }

        if filename == format!("{}.log", file_prefix) {
            continue;
        }

        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };

        let Ok(modified) = metadata.modified() else {
            continue;
        };

        let Ok(age) = now.duration_since(modified) else {
            continue;
        };

        if age <= max_age {
            continue;
        }

        let size = metadata.len();
        match fs::remove_file(&path) {
            Ok(_) => {
                tracing::info!(
                    file = %path.display(),
                    age_days = age.as_secs() / 86400,
                    size_bytes = size,
                    "Deleted old log file"
                );
                deleted_count += 1;
                total_size += size;
            }
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    "Failed to delete old log file"
                );
            }
        }
    }

    if deleted_count > 0 {
        tracing::info!(
            deleted_count = deleted_count,
            total_size_mb = total_size as f64 / (1024.0 * 1024.0),
            "Log cleanup complete"
        );
    }

    Ok(())
}

fn spawn_log_cleanup_task(log_dir: String, file_prefix: String, max_age_days: u64) {
    if max_age_days == 0 {
        return;
    }

    std::thread::spawn(move || {
        loop {
            // Run cleanup daily at 2 AM (or after 24 hours from start)
            std::thread::sleep(Duration::from_secs(24 * 60 * 60));

            if let Err(e) = cleanup_old_logs(&log_dir, &file_prefix, max_age_days) {
                eprintln!("Error cleaning up old logs: {}", e);
            }
        }
    });
}

pub fn init_tracing_with_config(config: LogConfig) -> Result<(), Box<dyn std::error::Error>> {
    let filter_layer = EnvFilter::try_new(&config.filter)?;

    let registry = tracing_subscriber::registry()
        .with(filter_layer)
        .with(ErrorLayer::default());

    match config.log_dir {
        Some(log_dir) => {
            // Clean up old logs immediately on startup
            cleanup_old_logs(&log_dir, &config.file_prefix, config.max_log_age_days)?;

            // Spawn background task for periodic cleanup
            spawn_log_cleanup_task(
                log_dir.clone(),
                config.file_prefix.clone(),
                config.max_log_age_days,
            );

            // File logging with rotation
            let file_appender =
                tracing_appender::rolling::daily(&log_dir, format!("{}.log", config.file_prefix));
            let (non_blocking_file, _guard) = tracing_appender::non_blocking(file_appender);

            if config.json_format {
                // JSON to file, plain to stdout
                let file_layer = fmt::layer()
                    .json()
                    .with_writer(non_blocking_file)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true);

                let stdout_layer = fmt::layer()
                    .compact()
                    .with_ansi(config.ansi)
                    .with_writer(std::io::stdout);

                registry.with(file_layer).with(stdout_layer).init();
            } else {
                // Plain format to both file and stdout
                let file_layer = fmt::layer()
                    .with_writer(non_blocking_file)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true);

                let stdout_layer = fmt::layer()
                    .compact()
                    .with_ansi(config.ansi)
                    .with_writer(std::io::stdout);

                registry.with(file_layer).with(stdout_layer).init();
            }

            // Guard needs to live for the entire program
            std::mem::forget(_guard);
        }
        None => {
            // Stdout only
            if config.json_format {
                let fmt_layer = fmt::layer()
                    .json()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true);

                registry.with(fmt_layer).init();
            } else {
                let fmt_layer = fmt::layer().compact().with_ansi(config.ansi);

                registry.with(fmt_layer).init();
            }
        }
    }

    Ok(())
}
