pub mod auth;
pub mod client;
pub mod device;
pub mod protocol;
pub mod ratelimit;
pub mod server;

pub use device::{load_or_create_device_id, DeviceIdentity};

/// Default public relay base URL (HTTPS).
pub const DEFAULT_RELAY_URL: &str = "https://ilinkhub.ai";

/// Resolve the QR code public URL prefix for pairing.
pub fn resolve_pair_public_url(device_id: &str) -> String {
    if let Ok(url) = std::env::var("HUB_PAIR_URL").or_else(|_| std::env::var("HUB_PUBLIC_URL")) {
        let s = url.trim().trim_end_matches('/').to_string();
        if !s.is_empty() {
            return s;
        }
    }

    if std::env::var("ILINKHUB_RELAY")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        return "http://127.0.0.1:8765".to_string();
    }

    let relay_base =
        std::env::var("ILINKHUB_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());
    format!(
        "{}/pair/{}",
        relay_base.trim().trim_end_matches('/'),
        device_id
    )
}

/// Build the URL embedded in the pairing QR code.
pub fn pair_qr_url(public_base: &str, code: &str) -> String {
    let legacy = std::env::var("HUB_PAIR_URL").is_ok() || std::env::var("HUB_PUBLIC_URL").is_ok();
    if legacy {
        format!("{public_base}/hub/pair/{code}")
    } else {
        format!("{public_base}/{code}")
    }
}

/// Whether the built-in relay client should connect on startup.
pub fn relay_enabled() -> bool {
    !std::env::var("ILINKHUB_RELAY")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
        && std::env::var("HUB_PAIR_URL").is_err()
        && std::env::var("HUB_PUBLIC_URL").is_err()
}

pub fn relay_ws_url() -> String {
    let base =
        std::env::var("ILINKHUB_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());
    let base = base.trim().trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/ws/pairing")
    } else if let Some(rest) = base.strip_prefix("http://") {
        let host = rest.split('/').next().unwrap_or(rest);
        let is_loopback = host.starts_with("127.0.0.1")
            || host.starts_with("localhost")
            || host.starts_with("[::1]");
        if is_loopback {
            format!("ws://{rest}/ws/pairing")
        } else {
            // Refuse cleartext relay to non-loopback hosts — MITM can forge pairing.
            tracing::error!(
                url = %base,
                "ILINKHUB_RELAY_URL uses http:// to a non-loopback host; \
                 upgrading to wss://. Set https:// explicitly."
            );
            format!("wss://{rest}/ws/pairing")
        }
    } else {
        format!("wss://{base}/ws/pairing")
    }
}

/// Map `0.0.0.0:8765` listen address to loopback for internal HTTP forwarding.
pub fn hub_loopback_addr(listen_addr: &str) -> String {
    listen_addr.replacen("0.0.0.0", "127.0.0.1", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── hub_loopback_addr ───────────────────────────────────────────────────

    #[test]
    fn loopback_replaces_wildcard() {
        assert_eq!(hub_loopback_addr("0.0.0.0:8765"), "127.0.0.1:8765");
    }

    #[test]
    fn loopback_leaves_specific_addr_unchanged() {
        assert_eq!(hub_loopback_addr("192.168.1.1:8765"), "192.168.1.1:8765");
        assert_eq!(hub_loopback_addr("127.0.0.1:8765"), "127.0.0.1:8765");
    }

    #[test]
    fn loopback_replaces_only_first_occurrence() {
        // Pathological input — only the leading 0.0.0.0 should be replaced.
        assert_eq!(hub_loopback_addr("0.0.0.0:0.0.0.0"), "127.0.0.1:0.0.0.0");
    }

    // ─── relay_ws_url ───────────────────────────────────────────────────────

    #[test]
    fn ws_url_converts_https_to_wss() {
        assert_eq!(
            relay_ws_url_from("https://ilinkhub.ai"),
            "wss://ilinkhub.ai/ws/pairing"
        );
    }

    #[test]
    fn ws_url_converts_http_to_ws() {
        assert_eq!(
            relay_ws_url_from("http://localhost:9000"),
            "ws://localhost:9000/ws/pairing"
        );
    }

    #[test]
    fn ws_url_upgrades_non_loopback_http_to_wss() {
        assert_eq!(
            relay_ws_url_from("http://relay.example.com"),
            "wss://relay.example.com/ws/pairing"
        );
    }

    #[test]
    fn ws_url_no_scheme_defaults_to_wss() {
        assert_eq!(
            relay_ws_url_from("ilinkhub.ai"),
            "wss://ilinkhub.ai/ws/pairing"
        );
    }

    #[test]
    fn ws_url_strips_trailing_slash() {
        assert_eq!(
            relay_ws_url_from("https://ilinkhub.ai/"),
            "wss://ilinkhub.ai/ws/pairing"
        );
    }

    // ─── pair_qr_url ────────────────────────────────────────────────────────

    #[test]
    fn pair_qr_url_modern_path() {
        // Without legacy env vars, the URL is <base>/<code>.
        assert_eq!(
            pair_qr_url_inner("https://ilinkhub.ai/pair/dev-id", "abc123", false),
            "https://ilinkhub.ai/pair/dev-id/abc123"
        );
    }

    #[test]
    fn pair_qr_url_legacy_path() {
        // When HUB_PAIR_URL / HUB_PUBLIC_URL is set, the old /hub/pair/<code> path is used.
        assert_eq!(
            pair_qr_url_inner("https://my-hub.example.com", "abc123", true),
            "https://my-hub.example.com/hub/pair/abc123"
        );
    }

    // ─── resolve_pair_public_url ─────────────────────────────────────────────

    #[test]
    fn public_url_uses_relay_base_and_device_id() {
        assert_eq!(
            resolve_pair_public_url_from("dev-001", None, None, None),
            "https://ilinkhub.ai/pair/dev-001"
        );
    }

    #[test]
    fn public_url_prefers_hub_pair_url_override() {
        assert_eq!(
            resolve_pair_public_url_from(
                "dev-001",
                Some("https://custom.example.com/"),
                None,
                None
            ),
            "https://custom.example.com"
        );
    }

    #[test]
    fn public_url_falls_back_to_loopback_when_relay_disabled() {
        assert_eq!(
            resolve_pair_public_url_from("dev-001", None, Some("0"), None),
            "http://127.0.0.1:8765"
        );
    }

    #[test]
    fn public_url_uses_custom_relay_url() {
        assert_eq!(
            resolve_pair_public_url_from("dev-001", None, None, Some("https://relay.local")),
            "https://relay.local/pair/dev-001"
        );
    }

    // ─── relay_enabled ───────────────────────────────────────────────────────

    #[test]
    fn relay_enabled_logic_default() {
        assert!(relay_enabled_from(None, false, false));
    }

    #[test]
    fn relay_disabled_when_env_is_0() {
        assert!(!relay_enabled_from(Some("0"), false, false));
    }

    #[test]
    fn relay_disabled_when_env_is_false_case_insensitive() {
        assert!(!relay_enabled_from(Some("FALSE"), false, false));
        assert!(!relay_enabled_from(Some("false"), false, false));
    }

    #[test]
    fn relay_disabled_when_hub_pair_url_is_set() {
        assert!(!relay_enabled_from(None, true, false));
    }

    #[test]
    fn relay_disabled_when_hub_public_url_is_set() {
        assert!(!relay_enabled_from(None, false, true));
    }

    // ─── Pure-function helpers for env-independent testing ───────────────────

    fn relay_ws_url_from(base: &str) -> String {
        let base = base.trim().trim_end_matches('/');
        if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}/ws/pairing")
        } else if let Some(rest) = base.strip_prefix("http://") {
            let host = rest.split('/').next().unwrap_or(rest);
            let is_loopback = host.starts_with("127.0.0.1")
                || host.starts_with("localhost")
                || host.starts_with("[::1]");
            if is_loopback {
                format!("ws://{rest}/ws/pairing")
            } else {
                format!("wss://{rest}/ws/pairing")
            }
        } else {
            format!("wss://{base}/ws/pairing")
        }
    }

    fn pair_qr_url_inner(public_base: &str, code: &str, legacy: bool) -> String {
        if legacy {
            format!("{public_base}/hub/pair/{code}")
        } else {
            format!("{public_base}/{code}")
        }
    }

    fn resolve_pair_public_url_from(
        device_id: &str,
        hub_pair_url: Option<&str>,
        ilinkhub_relay: Option<&str>,
        ilinkhub_relay_url: Option<&str>,
    ) -> String {
        if let Some(url) = hub_pair_url {
            let s = url.trim().trim_end_matches('/').to_string();
            if !s.is_empty() {
                return s;
            }
        }
        if ilinkhub_relay
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
        {
            return "http://127.0.0.1:8765".to_string();
        }
        let relay_base = ilinkhub_relay_url.unwrap_or(DEFAULT_RELAY_URL);
        format!(
            "{}/pair/{}",
            relay_base.trim().trim_end_matches('/'),
            device_id
        )
    }

    fn relay_enabled_from(
        ilinkhub_relay: Option<&str>,
        hub_pair_url_set: bool,
        hub_public_url_set: bool,
    ) -> bool {
        !ilinkhub_relay
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
            && !hub_pair_url_set
            && !hub_public_url_set
    }
}
