use serde::{Deserialize, Serialize};

pub const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
pub const ILINK_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// ─── Common ──────────────────────────────────────────────────────────────────

/// Attached to every outgoing CGI request per iLink protocol.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BaseInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_agent: Option<String>,
}

impl Default for BaseInfo {
    fn default() -> Self {
        Self {
            channel_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            bot_agent: Some(format!("ilink-hub/{}", env!("CARGO_PKG_VERSION"))),
        }
    }
}

// ─── Login / QR Code ────────────────────────────────────────────────────────

/// Response from `/ilink/bot/get_bot_qrcode`.
/// Actual API shape: {"ret":0,"qrcode":"<key>","qrcode_img_content":"https://..."}
#[derive(Debug, Serialize, Deserialize)]
pub struct GetQrcodeResponse {
    pub ret: i32,
    /// The QR code key / identifier used for polling.
    pub qrcode: Option<String>,
    /// The URL to render as a QR code (user scans this URL).
    pub qrcode_img_content: Option<String>,
    pub errmsg: Option<String>,
}

/// Response from `/ilink/bot/get_qrcode_status`.
/// Observed status values: "wait" | "scaned" | "confirmed" | "expired"
/// On "confirmed": also includes bot_token, baseurl, ilink_bot_id, ilink_user_id.
#[derive(Debug, Serialize, Deserialize)]
pub struct QrcodeStatusResponse {
    pub ret: i32,
    /// "wait" | "confirmed" | "expired" (string, not integer)
    pub status: Option<String>,
    pub bot_token: Option<String>,
    pub baseurl: Option<String>,
    pub ilink_bot_id: Option<String>,
    pub ilink_user_id: Option<String>,
    pub errmsg: Option<String>,
}

// ─── Message item types ──────────────────────────────────────────────────────

pub mod msg_type {
    pub const TEXT: i32 = 1;
    pub const IMAGE: i32 = 2;
    pub const VOICE: i32 = 3;
    pub const FILE: i32 = 4;
    pub const VIDEO: i32 = 5;
}

pub mod message_state {
    pub const FINISH: i32 = 2;
}

/// Text content inside a MessageItem.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Voice content inside a MessageItem (type=3).
/// `text` is the ASR transcript provided by WeChat, may be absent if recognition failed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Image content inside a MessageItem (type=2).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageItem {
    /// CDN URL for the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_url: Option<String>,
    /// MD5 hash of the image bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// media_id returned by getuploadurl (used when sending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<String>,
}

/// File content inside a MessageItem (type=4).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileItem {
    /// Original file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// CDN URL for the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_url: Option<String>,
    /// File size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// MD5 hash of the file bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// media_id returned by getuploadurl (used when sending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<String>,
}

/// Video content inside a MessageItem (type=5).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoItem {
    /// CDN URL for the video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_url: Option<String>,
    /// Duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// MD5 hash of the video bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// media_id returned by getuploadurl (used when sending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<String>,
}

/// One item inside a WeixinMessage's item_list.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageItem {
    /// Item type: 1=text, 2=image, 3=voice, 4=file, 5=video
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub item_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_item: Option<TextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_item: Option<VoiceItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_item: Option<ImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_item: Option<FileItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_item: Option<VideoItem>,
    /// Catch-all for unknown fields from iLink upstream.
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

// ─── Unified message type ────────────────────────────────────────────────────

/// The canonical message type used in both upstream (iLink wire protocol) and
/// the hub's downstream API (what agent backends receive and send).
///
/// Field names mirror the official iLink / openclaw-weixin SDK.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WeixinMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time_ms: Option<i64>,
    /// Present for group messages (group/session identifier).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 1 = user message, 2 = bot message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_state: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_list: Option<std::sync::Arc<Vec<MessageItem>>>,
    /// Required for routing replies back to the correct conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    /// ilink-hub 扩展元数据（Hub 与已注册后端之间专用，不会透传给官方 iLink 上游）。
    ///
    /// Hub 在转发消息给下游前注入此字段；下游回复时可在此字段中携带 `cli_session_id`
    /// 以告知 Hub 当前活跃的后端 session UUID（如 Claude Code `--resume` 的 UUID）。
    ///
    /// 使用官方 iLink SDK 的后端不感知此字段（忽略未知 JSON key）；
    /// 不支持 session 管理的后端同样可以正常收发消息，只是无法利用 session 连续性。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ilink_hub_ext: Option<HubExt>,
}

/// ilink-hub 专有扩展字段，封装于 `WeixinMessage.ilink_hub_ext`。
///
/// * **Hub → 下游**：`session_id`（当前活跃 session 的后端 UUID）、`session_name`（可读标识）
/// * **下游 → Hub**：`cli_session_id`（下游上报的后端 UUID，Hub 将其持久化到对应 session）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HubExt {
    /// Hub 注入：当前活跃 session 已持久化的后端 UUID（如 Claude `--resume` 值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Hub 注入：当前活跃 session 的可读名称（如 `"feature-a"`，默认 `"default"`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// 下游 → Hub：下游在 `sendmessage` 时填入，Hub 将其写入当前活跃 session 的存储。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_session_id: Option<String>,
}

impl WeixinMessage {
    /// Extract displayable text: prefers text_item, falls back to voice_item ASR transcript.
    pub fn text(&self) -> Option<&str> {
        let items = self.item_list.as_ref()?;
        items
            .iter()
            .find_map(|item| item.text_item.as_ref()?.text.as_deref())
            .or_else(|| {
                items
                    .iter()
                    .find_map(|item| item.voice_item.as_ref()?.text.as_deref())
            })
    }

    /// Return the `item_type` of the first item in the list, if any.
    pub fn first_item_type(&self) -> Option<i32> {
        self.item_list.as_ref()?.first()?.item_type
    }

    /// Return true if the message contains at least one non-empty item (any type).
    pub fn has_content(&self) -> bool {
        self.item_list
            .as_ref()
            .map(|l| !l.is_empty())
            .unwrap_or(false)
    }

    /// Build a text reply to this message.
    pub fn build_text_reply(context_token: String, text: String) -> WeixinMessage {
        let mut msg = WeixinMessage {
            context_token: Some(context_token),
            message_type: Some(2), // BOT
            message_state: Some(message_state::FINISH),
            from_user_id: Some(String::new()),
            client_id: Some(new_client_id()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(msg_type::TEXT),
                text_item: Some(TextItem { text: Some(text) }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        msg.ensure_outbound();
        msg
    }

    /// Build an image reply using a media_id obtained from `getuploadurl`.
    pub fn build_image_reply(context_token: String, media_id: String) -> WeixinMessage {
        let mut msg = WeixinMessage {
            context_token: Some(context_token),
            message_type: Some(2),
            message_state: Some(message_state::FINISH),
            from_user_id: Some(String::new()),
            client_id: Some(new_client_id()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(msg_type::IMAGE),
                image_item: Some(ImageItem {
                    media_id: Some(media_id),
                    ..Default::default()
                }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        msg.ensure_outbound();
        msg
    }

    /// Build a file reply using a media_id obtained from `getuploadurl`.
    pub fn build_file_reply(
        context_token: String,
        media_id: String,
        file_name: Option<String>,
    ) -> WeixinMessage {
        let mut msg = WeixinMessage {
            context_token: Some(context_token),
            message_type: Some(2),
            message_state: Some(message_state::FINISH),
            from_user_id: Some(String::new()),
            client_id: Some(new_client_id()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(msg_type::FILE),
                file_item: Some(FileItem {
                    media_id: Some(media_id),
                    file_name,
                    ..Default::default()
                }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        msg.ensure_outbound();
        msg
    }

    /// Normalize outbound fields per iLink protocol (empty from_user_id, unique client_id, FINISH state).
    pub fn ensure_outbound(&mut self) {
        self.from_user_id = Some(String::new());
        if self.message_type.is_none() {
            self.message_type = Some(2);
        }
        if self.message_state.is_none() {
            self.message_state = Some(message_state::FINISH);
        }
        if self
            .client_id
            .as_ref()
            .map(|s| s.is_empty())
            .unwrap_or(true)
        {
            self.client_id = Some(new_client_id());
        }
    }
}

fn new_client_id() -> String {
    format!("ilink-hub:{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod outbound_tests {
    use super::*;

    #[test]
    fn build_text_reply_sets_outbound_fields() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), "hi".to_string());
        assert_eq!(msg.from_user_id.as_deref(), Some(""));
        assert_eq!(msg.message_type, Some(2));
        assert_eq!(msg.message_state, Some(message_state::FINISH));
        assert!(msg.client_id.as_deref().unwrap().starts_with("ilink-hub:"));
    }

    #[test]
    fn ensure_outbound_assigns_unique_client_id() {
        let mut msg1 = WeixinMessage::default();
        let mut msg2 = WeixinMessage::default();
        msg1.ensure_outbound();
        msg2.ensure_outbound();
        assert_ne!(msg1.client_id, msg2.client_id);
    }
}

// ─── GetUpdates (getupdates endpoint) ────────────────────────────────────────

/// Request body for `POST /ilink/bot/getupdates`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GetUpdatesRequest {
    /// Long-poll cursor; send empty string on first call.
    #[serde(default)]
    pub get_updates_buf: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_info: Option<BaseInfo>,
    /// Long-poll seconds (0 = return immediately if no messages). Defaults to 30 on Hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
}

/// Response body for `POST /ilink/bot/getupdates`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret: Option<i32>,
    /// Server error code (e.g. -14 = session timeout). Present when request fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errcode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msgs: Option<Vec<WeixinMessage>>,
    /// Updated cursor to pass on next request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_updates_buf: Option<String>,
}

// ─── SendMessage ─────────────────────────────────────────────────────────────

/// Request body for `POST /ilink/bot/sendmessage`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SendMessageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<WeixinMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_info: Option<BaseInfo>,
}

impl SendMessageRequest {
    pub fn text(context_token: String, text: String) -> Self {
        Self {
            msg: Some(WeixinMessage::build_text_reply(context_token, text)),
            base_info: Some(BaseInfo::default()),
        }
    }

    /// Build a text reply to a WeChat user. `from_user_id` must be empty per iLink protocol.
    pub fn reply(context_token: String, text: String, to_user_id: &str) -> Self {
        Self::reply_text(context_token, text, to_user_id, None)
    }

    /// Same as [`reply`](Self::reply) but allows bridge to attach `cli_session_id` (via `ilink_hub_ext`) for Hub to persist.
    pub fn reply_text(
        context_token: String,
        text: String,
        to_user_id: &str,
        cli_session_id: Option<String>,
    ) -> Self {
        let mut msg = WeixinMessage::build_text_reply(context_token, text);
        if !to_user_id.is_empty() {
            msg.to_user_id = Some(to_user_id.to_string());
        }
        if cli_session_id.is_some() {
            msg.ilink_hub_ext = Some(HubExt {
                cli_session_id,
                ..Default::default()
            });
        }
        Self {
            msg: Some(msg),
            base_info: Some(BaseInfo::default()),
        }
    }
}

/// Response body for `POST /ilink/bot/sendmessage`.
/// The real iLink API returns an empty body on success; ret/errmsg added by hub.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SendMessageResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
}

impl SendMessageResponse {
    pub fn ok() -> Self {
        Self {
            ret: Some(0),
            errmsg: None,
        }
    }
    pub fn err(code: i32, msg: impl Into<String>) -> Self {
        Self {
            ret: Some(code),
            errmsg: Some(msg.into()),
        }
    }
}

// ─── GetConfig ───────────────────────────────────────────────────────────────

/// Request body for `POST /ilink/bot/getconfig`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct GetConfigRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ilink_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_info: Option<BaseInfo>,
}

/// Response body for `POST /ilink/bot/getconfig`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct GetConfigResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
    /// Base64-encoded typing ticket for sendTyping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_ticket: Option<String>,
}

// ─── SendTyping ──────────────────────────────────────────────────────────────

/// Request body for `POST /ilink/bot/sendtyping`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SendTypingRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ilink_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_ticket: Option<String>,
    /// 1 = typing (default), 2 = cancel typing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_info: Option<BaseInfo>,
}

// ─── Media Upload ─────────────────────────────────────────────────────────────

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
