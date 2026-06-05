use serde::{Deserialize, Serialize};

/// Standard iLink API request headers.
/// Clients must send `X-WECHAT-UIN` (random base64) and `Authorization: Bearer <token>`.
pub const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
pub const ILINK_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// ─── Login / QR Code ────────────────────────────────────────────────────────

/// Response from `/ilink/bot/get_bot_qrcode`.
/// Actual API shape:
///   {"ret":0,"qrcode":"<key>","qrcode_img_content":"https://..."}
#[derive(Debug, Deserialize)]
pub struct GetQrcodeResponse {
    pub ret: i32,
    /// The QR code key / identifier used for polling.
    pub qrcode: Option<String>,
    /// The URL to render as a QR code (user scans this URL).
    pub qrcode_img_content: Option<String>,
    pub errmsg: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QrcodeStatusResponse {
    pub ret: i32,
    pub status: Option<i32>,
    pub bot_token: Option<String>,
    pub errmsg: Option<String>,
}

// ─── Updates (getupdates) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUpdatesRequest {
    /// Last-seen message cursor (returned by previous call)
    pub buf: Option<String>,
    /// Timeout in seconds (long-poll duration)
    pub timeout: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUpdatesResponse {
    pub ret: i32,
    pub errmsg: Option<String>,
    /// Updated cursor to pass on next call
    pub buf: Option<String>,
    pub list: Option<Vec<InboundMessage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub msg_id: String,
    pub from_user: String,
    pub chat_id: Option<String>,
    /// "direct" | "group"
    pub chat_type: Option<String>,
    pub msg_type: i32,
    pub content: Option<String>,
    pub context_token: String,
    pub timestamp: Option<i64>,
    /// Additional metadata (image/file info etc.)
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

// ─── Send Message ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub context_token: String,
    pub msg_type: i32,
    pub content: Option<String>,
    /// For media messages
    pub media_id: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub ret: i32,
    pub errmsg: Option<String>,
}

// ─── Typing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct GetConfigRequest {
    pub context_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetConfigResponse {
    pub ret: i32,
    pub typing_ticket: Option<String>,
    pub errmsg: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendTypingRequest {
    pub context_token: String,
    pub typing_ticket: String,
}

// ─── Media Upload ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct GetUploadUrlRequest {
    pub file_type: String,
    pub file_size: u64,
    pub file_md5: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetUploadUrlResponse {
    pub ret: i32,
    pub upload_url: Option<String>,
    pub media_id: Option<String>,
    pub errmsg: Option<String>,
}

// ─── Message types ───────────────────────────────────────────────────────────

pub mod msg_type {
    pub const TEXT: i32 = 1;
    pub const IMAGE: i32 = 3;
    pub const FILE: i32 = 6;
    pub const VIDEO: i32 = 43;
    pub const VOICE: i32 = 34;
}
