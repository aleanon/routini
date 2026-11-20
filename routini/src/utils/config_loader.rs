//! Finds, reads and validates the proxy [`Config`](crate::config::Config), in JSON or TOML.
//!
//! The format is taken from the file extension (`.json`, `.toml`). For any other extension the
//! content decides: a document starting with `{` is JSON, anything else is TOML.
//!
//! The file is looked up in this order:
//! 1. `--config <path>` on the command line
//! 2. the `ROUTINI_CONFIG` environment variable
//! 3. `config.toml` or `config.json` in the working directory (having both is an error, so it is
//!    never ambiguous which one is used)
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::config::Config;

/// Environment variable that overrides the default config path.
pub const CONFIG_PATH_ENV: &str = "ROUTINI_CONFIG";
/// Looked for in the working directory when no path is given.
pub const DEFAULT_CONFIG_PATHS: [&str; 2] = ["config.toml", "config.json"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Toml,
}

impl ConfigFormat {
    /// By extension, falling back to sniffing the content.
    pub fn detect(path: &Path, contents: &str) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("json") => ConfigFormat::Json,
            Some("toml") => ConfigFormat::Toml,
            _ if contents.trim_start().starts_with('{') => ConfigFormat::Json,
            _ => ConfigFormat::Toml,
        }
    }
}

/// Pick the config file: CLI flag, then `ROUTINI_CONFIG`, then whichever of
/// [DEFAULT_CONFIG_PATHS] exists in `dir`.
pub fn resolve_config_path(
    cli_path: Option<PathBuf>,
    env_path: Option<String>,
    dir: &Path,
) -> Result<PathBuf> {
    if let Some(path) = cli_path {
        return Ok(path);
    }
    if let Some(path) = env_path.filter(|p| !p.trim().is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let found: Vec<PathBuf> = DEFAULT_CONFIG_PATHS
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .collect();
    match found.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(eyre!(
            "No config file found: pass --config <path>, set {CONFIG_PATH_ENV}, or create {} in {}",
            DEFAULT_CONFIG_PATHS.join(" or "),
            dir.display()
        )),
        _ => Err(eyre!(
            "Both {} exist in {}: pass --config <path> or set {CONFIG_PATH_ENV} to choose one",
            DEFAULT_CONFIG_PATHS.join(" and "),
            dir.display()
        )),
    }
}

/// [resolve_config_path] using the real `ROUTINI_CONFIG` and working directory.
pub fn config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    resolve_config_path(
        cli_path,
        std::env::var(CONFIG_PATH_ENV).ok(),
        &std::env::current_dir()?,
    )
}

/// Read, parse (format auto-detected), apply environment overrides and validate the config at
/// `path`. Every validation problem is reported at once.
pub fn load_config_from<P: AsRef<Path>>(path: P) -> Result<Config> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read config file {}", path.display()))?;
    let format = ConfigFormat::detect(path, &contents);
    let mut config = Config::parse(&contents, format).wrap_err_with(|| {
        format!(
            "Failed to parse config file {} as {format:?}",
            path.display()
        )
    })?;
    // Canonical, so the same file gives the same config however its path was written
    // (relative at startup, absolute on reload)
    let config_dir = path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    config.resolve_paths(&config_dir);
    config.apply_env_overrides(|key| std::env::var(key).ok());
    config
        .validate()
        .wrap_err_with(|| format!("Invalid config file {}", path.display()))?;
    Ok(config)
}

/// Load the config from [`config_path`] without a CLI override.
pub fn load_config() -> Result<Config> {
    load_config_from(config_path(None)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("routini-cfgload-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn format_from_extension_then_content() {
        let detect = |path: &str, contents: &str| ConfigFormat::detect(Path::new(path), contents);
        assert_eq!(detect("a.json", "proxy = 1"), ConfigFormat::Json);
        assert_eq!(detect("a.TOML", "{"), ConfigFormat::Toml);
        assert_eq!(
            detect("routini.conf", "  \n {\"proxy\": {}}"),
            ConfigFormat::Json
        );
        assert_eq!(
            detect("routini.conf", "[proxy]\nlistener = 1"),
            ConfigFormat::Toml
        );
        assert_eq!(detect("noext", "# comment\n[proxy]"), ConfigFormat::Toml);
    }

    #[test]
    fn lookup_order() {
        let dir = temp_dir("order");
        let cli = Some(PathBuf::from("cli.toml"));
        let env = Some("env.json".to_string());
        assert_eq!(
            resolve_config_path(cli, env.clone(), &dir).unwrap(),
            PathBuf::from("cli.toml")
        );
        assert_eq!(
            resolve_config_path(None, env, &dir).unwrap(),
            PathBuf::from("env.json")
        );

        let err = resolve_config_path(None, Some(" ".into()), &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("No config file found"), "{err}");

        std::fs::write(dir.join("config.json"), "{}").unwrap();
        assert_eq!(
            resolve_config_path(None, None, &dir).unwrap(),
            dir.join("config.json")
        );

        std::fs::write(dir.join("config.toml"), "").unwrap();
        let err = resolve_config_path(None, None, &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Both"), "{err}");
    }

    #[test]
    fn load_reports_format_and_validation_errors() {
        let dir = temp_dir("load");
        let toml = dir.join("bad.toml");
        std::fs::write(&toml, "[proxy]\nlistener = 3500\nrouter = []\n").unwrap();
        let err = format!("{:#}", load_config_from(&toml).unwrap_err());
        assert!(err.contains("at least one route"), "{err}");

        let json = dir.join("broken.json");
        std::fs::write(&json, "{ not json").unwrap();
        let err = format!("{:#}", load_config_from(&json).unwrap_err());
        assert!(err.contains("as Json"), "{err}");
    }
}
