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
        format!("ws://{rest}/ws/pairing")
    } else {
        format!("wss://{base}/ws/pairing")
    }
}

/// Map `0.0.0.0:8765` listen address to loopback for internal HTTP forwarding.
pub fn hub_loopback_addr(listen_addr: &str) -> String {
    listen_addr.replacen("0.0.0.0", "127.0.0.1", 1)
}
