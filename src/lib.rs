pub mod bridge;
pub mod client;
pub mod error;
pub mod hub;
pub mod ilink;
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
}
