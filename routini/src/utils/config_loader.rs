//! Loads the proxy [`Config`](crate::config::Config) from a JSON file.
use std::path::Path;

use color_eyre::eyre::{Context, Result};

use crate::config::Config;

/// Environment variable that overrides the default config path.
pub const CONFIG_PATH_ENV: &str = "ROUTINI_CONFIG";
/// Default config path used when [`CONFIG_PATH_ENV`] is unset.
pub const DEFAULT_CONFIG_PATH: &str = "config.json";

/// Load and parse the config from `path`.
pub fn load_config_from<P: AsRef<Path>>(path: P) -> Result<Config> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read config file {}", path.display()))?;
    let config = serde_json::from_str(&contents)
        .wrap_err_with(|| format!("Failed to parse config file {}", path.display()))?;
    Ok(config)
}

/// Load the config from [`CONFIG_PATH_ENV`] if set, otherwise [`DEFAULT_CONFIG_PATH`].
pub fn load_config() -> Result<Config> {
    let path = std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    load_config_from(path)
}
