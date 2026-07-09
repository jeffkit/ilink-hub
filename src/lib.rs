pub mod bridge;
pub mod client;
pub mod error;
pub mod hub;
pub mod ilink;
pub mod mcp;
pub mod metrics;
pub mod paths;
pub mod relay;
pub mod runtime;
pub mod server;
pub mod store;

pub use error::HubError;
pub use hub::queue::InMemoryQueue;
pub use hub::queue::MessageQueue;
pub use hub::HubState;
pub use ilink::QrLoginUiEvent;
pub use runtime::serve::{run_serve, ServeOptions};

/// Redact a virtual token for logging: show only the first 8 characters followed by `…`.
/// This lets operators correlate log lines without exposing the full credential.
/// Safe against UTF-8 byte boundary panics.
pub fn redact_token(t: &str) -> String {
    let prefix: String = t.chars().take(8).collect();
    format!("{prefix}…")
}

/// Redact credentials in a database URL for startup logs.
///
/// Keeps scheme, host, path/db name, and username; replaces any password with
/// `***`. Unparseable strings (e.g. some SQLite DSNs) are returned unchanged
/// when they do not contain a `user:password@` authority segment.
pub fn redact_database_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                let _ = parsed.set_password(Some("***"));
            }
            parsed.to_string()
        }
        Err(_) => {
            // Fallback for non-standard DSNs: mask `scheme://user:secret@rest`.
            if let Some(scheme_sep) = url.find("://") {
                let after_scheme = &url[scheme_sep + 3..];
                if let Some(at) = after_scheme.find('@') {
                    let creds = &after_scheme[..at];
                    if let Some(colon) = creds.find(':') {
                        let user = &creds[..colon];
                        return format!(
                            "{}://{}:***@{}",
                            &url[..scheme_sep],
                            user,
                            &after_scheme[at + 1..]
                        );
                    }
                }
            }
            url.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_token_safety() {
        // Empty string
        assert_eq!(redact_token(""), "…");
        // Short ASCII
        assert_eq!(redact_token("abc"), "abc…");
        // Exact 8 ASCII
        assert_eq!(redact_token("12345678"), "12345678…");
        // Long ASCII
        assert_eq!(redact_token("abcdefghijkl"), "abcdefgh…");
        // Multi-byte Unicode (each emoji/char is multi-byte)
        // 🦀 is 4 bytes. Slicing at index 8 would split the 3rd crab and panic in byte-based slicing.
        assert_eq!(redact_token("🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀"), "🦀🦀🦀🦀🦀🦀🦀🦀…");
        assert_eq!(redact_token("测试token长度校验"), "测试token长…");
    }

    #[test]
    fn test_redact_database_url_masks_password() {
        let redacted = redact_database_url("mysql://user:secret@host/db");
        assert!(
            !redacted.contains("secret"),
            "password must not appear in redacted URL: {redacted}"
        );
        assert!(
            redacted.contains("***"),
            "expected *** placeholder: {redacted}"
        );
        assert!(
            redacted.contains("mysql://"),
            "scheme preserved: {redacted}"
        );
        assert!(redacted.contains("host"), "host preserved: {redacted}");
        assert!(redacted.contains("db"), "db name preserved: {redacted}");
        assert!(redacted.contains("user"), "username preserved: {redacted}");
    }

    #[test]
    fn test_redact_database_url_no_password_unchanged_shape() {
        let url = "mysql://user@host/db";
        let redacted = redact_database_url(url);
        assert!(!redacted.contains("***"));
        assert!(redacted.contains("mysql://"));
        assert!(redacted.contains("host"));
    }

    #[test]
    fn test_redact_database_url_sqlite_memory() {
        // Common local DSN — must not panic and must not invent a password mask.
        let redacted = redact_database_url("sqlite::memory:");
        assert_eq!(redacted, "sqlite::memory:");
    }
}
