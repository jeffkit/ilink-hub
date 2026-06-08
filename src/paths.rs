//! Canonical user data paths under `~/.ilink-hub/`.
//!
//! Hub and bridge default to these locations so behavior does not depend on the
//! process current working directory.

use std::path::PathBuf;

/// `~/.ilink-hub` (or `./.ilink-hub` when home is unavailable).
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ilink-hub")
}

/// Default SQLite `DATABASE_URL`: `sqlite:~/.ilink-hub/ilink-hub.db`.
pub fn default_database_url() -> String {
    let db = data_dir().join("ilink-hub.db");
    format!("sqlite:{}", db.display())
}

/// Default bridge YAML config: `~/.ilink-hub/ilink-hub-bridge.yaml`.
pub fn default_bridge_config_path() -> PathBuf {
    data_dir().join("ilink-hub-bridge.yaml")
}

/// Default bridge credentials JSON: `~/.ilink-hub/bridge-credentials.json`.
pub fn default_bridge_credentials_path() -> PathBuf {
    data_dir().join("bridge-credentials.json")
}

/// Expand a leading `~` or `$HOME` in a config path (YAML `cwd`, `script`, etc.).
/// Returns the input unchanged when home is unavailable or no expansion applies.
pub fn expand_user_path(path: &str) -> String {
    let path = path.trim();
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if let Some(rest) = path.strip_prefix("$HOME/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if path == "$HOME" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_database_url_under_data_dir() {
        let url = default_database_url();
        assert!(url.starts_with("sqlite:"));
        assert!(url.contains(".ilink-hub"));
        assert!(url.ends_with("ilink-hub.db"));
    }

    #[test]
    fn bridge_defaults_live_under_data_dir() {
        let base = data_dir();
        assert_eq!(
            default_bridge_config_path(),
            base.join("ilink-hub-bridge.yaml")
        );
        assert_eq!(
            default_bridge_credentials_path(),
            base.join("bridge-credentials.json")
        );
    }

    #[test]
    fn expand_user_path_tilde() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(expand_user_path("~/foo"), home.join("foo").to_string_lossy());
        assert_eq!(expand_user_path("~"), home.to_string_lossy());
    }
}
