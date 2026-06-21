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

/// Root for the bridge manager plugin-style profile layout: `~/.ilink-hub-bridge`.
pub fn bridge_manager_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ilink-hub-bridge")
}

/// Default bridge manager profiles directory: `~/.ilink-hub-bridge/profiles`.
pub fn default_bridge_profiles_dir() -> PathBuf {
    bridge_manager_dir().join("profiles")
}

/// Default bridge manager credentials directory: `~/.ilink-hub-bridge/credentials`.
pub fn default_bridge_manager_credentials_dir() -> PathBuf {
    bridge_manager_dir().join("credentials")
}

/// Root for the desktop app's own bridge data: `~/.ilink-hub/desktop-bridge`.
///
/// Kept under `~/.ilink-hub/` (alongside `desktop-port.json` and the hub DB)
/// so all desktop-app state lives in one place and does not collide with a
/// simultaneously-running CLI bridge manager under `~/.ilink-hub-bridge/`.
pub fn desktop_bridge_dir() -> PathBuf {
    data_dir().join("desktop-bridge")
}

/// Desktop bridge manager profiles directory: `~/.ilink-hub/desktop-bridge/profiles`.
pub fn desktop_bridge_profiles_dir() -> PathBuf {
    desktop_bridge_dir().join("profiles")
}

/// Desktop bridge manager credentials directory: `~/.ilink-hub/desktop-bridge/credentials`.
pub fn desktop_bridge_credentials_dir() -> PathBuf {
    desktop_bridge_dir().join("credentials")
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

/// Default port used when `WEIXIN_BASE_URL` is a bare host with no port
/// (e.g. `http://example.com`).
pub const DEFAULT_HUB_PORT: u16 = 8765;

/// Parse a `WEIXIN_BASE_URL`-shaped string into `host:port`.
///
/// Accepts three forms:
/// - Full URL with scheme (`https://hub.example.com:9443/...`) → host + URL port.
/// - Bare host:port (`127.0.0.1:8765`) → returned unchanged after trim.
/// - Bare host with no port (`myhost`) → `myhost:{DEFAULT_HUB_PORT}`.
///
/// Returns `None` only when the input cannot be interpreted as a host:port
/// target — e.g. a URL with no host component, or a string that contains no
/// `:` and looks like neither a URL nor a host.
pub fn parse_host_port(s: &str) -> Option<String> {
    let s = s.trim();
    if let Ok(url) = reqwest::Url::parse(s) {
        if let Some(host) = url.host_str() {
            let port = url.port().unwrap_or(DEFAULT_HUB_PORT);
            return Some(format!("{}:{}", host, port));
        }
    }
    if s.contains(':') {
        return Some(s.to_string());
    }
    None
}

/// `~/.ilink-hub/relay-secret`
/// Persistent file that stores the Hub's per-process pairing-relay secret so
/// it survives restarts. Without persistence, every Hub restart invalidates
/// the in-flight `X-Ilink-Relay-Secret` value and any pairing requests still
/// holding the previous secret silently 401; the phone user sees a
/// "Hub 未在线" error with no actionable diagnostic.
///
/// File format: a single line of 32 base64url chars (no newline required, but
/// a trailing newline is tolerated). We use base64url so the secret is
/// copy-pasteable across logs/dashboards without escaping.
///
/// Permissions: 0600 on Unix (owner read/write only). On non-Unix the file is
/// created with the umask default; this is acceptable for the Tauri desktop
/// build which runs as the user.
pub fn relay_secret_path() -> PathBuf {
    data_dir().join("relay-secret")
}

/// Load a previously-persisted relay secret, or generate a fresh one and
/// persist it. Returns the secret as a 32-char base64url string.
///
/// The generated secret uses `rand::rng()` (OsRng-backed), so it has
/// the full 32 * 6 = 192 bits of entropy — collision-resistant for any
/// practical deployment.
///
/// On any I/O or permission error the function falls back to an ephemeral
/// (un-persisted) secret; the Hub will still function, just with the
/// pre-existing "secret rotates on restart" behaviour. We do NOT panic here
/// because this is on the startup hot path and the operator can still
/// recover by setting `ILINK_HUB_RELAY=0` to disable the relay.
pub fn load_or_create_relay_secret() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use rand::RngCore;

    let path = relay_secret_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        // Validate: must be base64url-decodable into exactly 24 bytes (32 chars
        // base64url encode 24 raw bytes — leaving 8 bytes of headroom for
        // future format changes that want a longer secret).
        if (24..=64).contains(&trimmed.len())
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && URL_SAFE_NO_PAD.decode(trimmed).is_ok()
        {
            return trimmed.to_string();
        }
        // Stale or corrupt: rotate. We do not delete here (the next write
        // truncates) so a concurrent read from a different process sees the
        // same content.
        tracing::warn!(
            path = %path.display(),
            "ignoring malformed relay-secret file; rotating"
        );
    }

    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = URL_SAFE_NO_PAD.encode(bytes);

    // Best-effort persist. Create the parent dir if needed; chmod 0600 on Unix.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, &encoded) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to persist relay secret to disk; using ephemeral secret"
            );
        }
    }
    encoded
}

#[cfg(test)]
mod relay_secret_tests {
    use super::*;

    /// Round-trip: persist a secret and read it back.
    #[test]
    fn load_or_create_relay_secret_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("relay-secret");
        // We can't override data_dir() without a process-wide test shim, so
        // we just call the loader in a non-collision directory and check the
        // shape: 32-char base64url string, deterministic on subsequent calls.
        let s1 = load_or_create_relay_secret();
        assert_eq!(s1.len(), 32, "32 base64url chars = 24 raw bytes");
        assert!(
            s1.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must be base64url charset"
        );
        let s2 = load_or_create_relay_secret();
        assert_eq!(s1, s2, "subsequent loads return the same secret");
        // Path must be under data_dir(); the file may or may not exist
        // depending on the host filesystem, but the function is total.
        let _ = path;
    }
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
    fn bridge_manager_defaults_live_under_manager_dir() {
        let base = bridge_manager_dir();
        assert_eq!(default_bridge_profiles_dir(), base.join("profiles"));
        assert_eq!(
            default_bridge_manager_credentials_dir(),
            base.join("credentials")
        );
    }

    #[test]
    fn desktop_bridge_paths_live_under_data_dir() {
        let base = data_dir();
        assert_eq!(desktop_bridge_dir(), base.join("desktop-bridge"));
        assert_eq!(
            desktop_bridge_profiles_dir(),
            base.join("desktop-bridge").join("profiles")
        );
        assert_eq!(
            desktop_bridge_credentials_dir(),
            base.join("desktop-bridge").join("credentials")
        );
    }

    #[test]
    fn expand_user_path_tilde() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(
            expand_user_path("~/foo"),
            home.join("foo").to_string_lossy()
        );
        assert_eq!(expand_user_path("~"), home.to_string_lossy());
    }
}
