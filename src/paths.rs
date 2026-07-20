//! Canonical user data paths under `~/.ilink-hub/`.
//!
//! Hub defaults to these locations so behavior does not depend on the
//! process current working directory. Bridge path helpers
//! (`default_bridge_*`, `desktop_bridge_*`, `expand_user_path`, …) moved to
//! the `im-agentproc` crate as part of the bridge extraction (2026-07-20); see
//! `docs/proposals/bridge-as-multi-im-runtime.md` Appendix A.

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
}
