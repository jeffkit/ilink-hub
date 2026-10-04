use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

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
            // 对齐官方 @tencent-weixin/openclaw-weixin SDK 的 channel_version。
            // 服务端按 channel_version 做能力门控：上报 Hub 自己的 0.x 版本号
            // 会导致媒体消息（图片/视频/文件）sendmessage 返回 ret=-2
            // "prepare failed"（文字不受影响）。
            channel_version: Some(ILINK_CHANNEL_VERSION.to_string()),
            bot_agent: Some(format!("ilink-hub/{}", env!("CARGO_PKG_VERSION"))),
        }
    }
}

/// 上游协议渠道版本。与官方 SDK（openclaw-weixin v2.4.6）保持一致，
/// 避免被服务端按老版本门控媒体能力。
pub const ILINK_CHANNEL_VERSION: &str = "2.4.6";

/// `iLink-App-ClientVersion` 请求头的编码：0x00MMNNPP
/// （major<<16 | minor<<8 | patch），与官方 SDK 的 buildClientVersion 一致。
pub const fn pack_client_version(major: u32, minor: u32, patch: u32) -> u32 {
    ((major & 0xff) << 16) | ((minor & 0xff) << 8) | (patch & 0xff)
}

/// 由 ILINK_CHANNEL_VERSION 编码的客户端版本号（编译期计算）。
pub const ILINK_APP_CLIENT_VERSION: u32 = pack_client_version(2, 4, 6);

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

/// CDN 媒体引用（AES-128-ECB 加密上传后的引用参数）。
/// 对齐 openclaw-weixin SDK 的 `CDNMedia`。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CDNMedia {
    /// CDN 上传响应头 x-encrypted-param 返回的加密参数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_query_param: Option<String>,
    /// base64(hex(aes_key)) —— 注意是 hex 字符串再 base64
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key: Option<String>,
    /// 固定 1（AES 加密标记）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_type: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_url: Option<String>,
}

/// Image content inside a MessageItem (type=2).
///
/// 出站（发图）走 openclaw-weixin 协议：`media: CDNMedia` + `mid_size`（密文大小）。
/// 旧字段（cdn_url/md5/media_id）保留做入站兼容。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageItem {
    /// 加密上传后的 CDN 引用（发送图片必需）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<CDNMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_media: Option<CDNMedia>,
    /// 密文字节数（发送时必须与上传的密文一致）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_size: Option<u64>,
    // ── 旧字段（入站兼容保留，出站不再使用）──
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
    /// Hub → 下游方向的投递序号（per-vtoken 单调、从 1 起），Hub 在入队时赋值，
    /// 同一消息重投时保持不变。上游（iLink）方向的该字段不再被 Hub 依赖。
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
    /// A2A call identifier.  Set by Hub on the inbound message to the target Agent;
    /// the target echoes it back in its `sendmessage` so Hub can resolve the waiter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a_call_id: Option<String>,
    /// A2A call chain depth (0 = direct user message, N = N levels deep in A2A calls).
    /// Set by Hub on synthetic A2A inbound messages; Bridge echoes it back in sendmessage.
    /// Hub rejects `call_agent` when depth >= MAX_A2A_DEPTH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a_depth: Option<u8>,
    /// Bridge → Hub: optional AgentProc 0.4 `usage` object from the terminal
    /// `result` / `error` event (token/cost stats). Hub MAY persist for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
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

    /// Return true if the message contains at least one non-text media item (image / voice / file / video).
    ///
    /// Unlike [`has_content`], this returns `false` for a text-only item_list even when the text
    /// is empty. Used by the sendmessage handler to distinguish session-persist-only messages
    /// (empty TextItem) from real media replies that have no text but do carry content.
    pub fn has_media_content(&self) -> bool {
        self.item_list
            .as_ref()
            .map(|l| {
                l.iter()
                    .any(|item| !matches!(item.item_type, Some(msg_type::TEXT) | None))
            })
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
        // Assign a Hub-generated `message_id` that iLink preserves verbatim and
        // echoes back as `ref_msg.message_item.msg_id` on quote-reply (verified
        // against the live iLink service). Persisted in `messages.ilink_msg_id`
        // at send time so inbound quote-replies can route to the exact backend/
        // session (L0 exact match) instead of the ±10s timestamp fallback.
        if self.message_id.is_none() {
            self.message_id = Some(new_outbound_msg_id());
        }
    }
}

/// Restart-safe, Hub-generated outbound `message_id` (i64).
///
/// Structure: `unix_millis * 1_000_000 + counter % 1_000_000`.
/// * `unix_millis` (~1.78e12) advances across restarts, so two processes (or a
///   restarted hub) never collide on the same id;
/// * the 20-bit counter slot disambiguates multiple sends within the same ms;
/// * magnitude ~1.78e18 stays well under `i64::MAX` (9.22e18) and in the range
///   confirmed to be preserved by iLink.
fn new_outbound_msg_id() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) % 1_000_000;
    (millis * 1_000_000 + n) as i64
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

    // ── has_media_content ────────────────────────────────────────────────────

    /// 空文本消息（session-persist-only）不含 media 内容。
    /// 这是发现 duplicate-reply bug 的核心断言：`build_text_reply("")` 会建出一个含空
    /// TextItem 的 item_list，has_content() 对此返回 true，但 has_media_content() 必须
    /// 返回 false，否则 sendmessage 的早返回保护失效，footer 被追加到空消息后发出。
    #[test]
    fn has_media_content_false_for_empty_text_reply() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), String::new());
        assert!(
            !msg.has_media_content(),
            "空 TextItem 不应视为 media content"
        );
    }

    #[test]
    fn has_media_content_false_for_nonempty_text_reply() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), "hello".to_string());
        assert!(!msg.has_media_content(), "纯文本回复不含 media content");
    }

    #[test]
    fn has_media_content_true_for_image_reply() {
        let msg = WeixinMessage::build_image_reply("ctx".to_string(), "media-id-abc".to_string());
        assert!(msg.has_media_content(), "图片回复应视为有 media content");
    }

    #[test]
    fn has_media_content_true_for_file_reply() {
        let msg = WeixinMessage::build_file_reply(
            "ctx".to_string(),
            "media-id-xyz".to_string(),
            Some("report.pdf".to_string()),
        );
        assert!(msg.has_media_content(), "文件回复应视为有 media content");
    }

    #[test]
    fn has_media_content_false_for_empty_item_list() {
        let msg = WeixinMessage::default();
        assert!(
            !msg.has_media_content(),
            "无 item_list 时不含 media content"
        );
    }

    /// 保证旧的 has_content() 对空 TextItem 仍返回 true（语义未变）。
    /// sendmessage handler 已改用 has_media_content()；此测试记录 has_content() 的现有行为
    /// 避免未来误改其语义影响其他调用方。
    #[test]
    fn has_content_true_for_empty_text_item() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), String::new());
        assert!(
            msg.has_content(),
            "has_content() 对含空 TextItem 的 item_list 应返回 true（记录现有行为）"
        );
    }

    // ── WeixinMessage::text() ────────────────────────────────────────────────

    #[test]
    fn text_returns_text_item_content() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), "hello".to_string());
        assert_eq!(msg.text(), Some("hello"));
    }

    #[test]
    fn text_falls_back_to_voice_item_asr_transcript() {
        let msg = WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(msg_type::VOICE),
                voice_item: Some(VoiceItem {
                    text: Some("voice asr text".to_string()),
                }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        assert_eq!(
            msg.text(),
            Some("voice asr text"),
            "text() must fall back to voice_item ASR transcript"
        );
    }

    #[test]
    fn text_returns_none_when_no_item_list() {
        let msg = WeixinMessage::default();
        assert!(msg.text().is_none());
    }

    #[test]
    fn text_returns_none_when_text_item_has_no_text_field_and_no_voice_fallback() {
        let msg = WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(msg_type::IMAGE),
                image_item: Some(crate::ilink::types::ImageItem {
                    cdn_url: Some("https://cdn.example.com/img.jpg".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        assert!(
            msg.text().is_none(),
            "image-only message must return None from text()"
        );
    }

    // ── WeixinMessage::first_item_type() ─────────────────────────────────────

    #[test]
    fn first_item_type_returns_type_of_first_item() {
        let msg = WeixinMessage::build_text_reply("ctx".to_string(), "hi".to_string());
        assert_eq!(msg.first_item_type(), Some(msg_type::TEXT));
    }

    #[test]
    fn first_item_type_returns_none_when_no_item_list() {
        let msg = WeixinMessage::default();
        assert!(msg.first_item_type().is_none());
    }

    // ── SendMessageRequest::reply_text() ─────────────────────────────────────

    #[test]
    fn reply_text_sets_to_user_id_when_non_empty() {
        let req =
            SendMessageRequest::reply_text("ctx".to_string(), "hi".to_string(), "user-123", None);
        let to_user = req.msg.as_ref().unwrap().to_user_id.as_deref();
        assert_eq!(
            to_user,
            Some("user-123"),
            "non-empty to_user_id must be set on msg"
        );
    }

    #[test]
    fn reply_text_does_not_set_to_user_id_when_empty() {
        let req = SendMessageRequest::reply_text("ctx".to_string(), "hi".to_string(), "", None);
        let to_user = req.msg.as_ref().unwrap().to_user_id.as_deref();
        assert!(to_user.is_none(), "empty to_user_id must NOT be set on msg");
    }

    #[test]
    fn reply_text_sets_ilink_hub_ext_when_cli_session_id_provided() {
        let req = SendMessageRequest::reply_text(
            "ctx".to_string(),
            "hi".to_string(),
            "",
            Some("session-uuid-abc".to_string()),
        );
        let ext = req.msg.as_ref().unwrap().ilink_hub_ext.as_ref();
        assert!(ext.is_some(), "cli_session_id must set ilink_hub_ext");
        assert_eq!(
            ext.unwrap().cli_session_id.as_deref(),
            Some("session-uuid-abc")
        );
    }

    // ── SendMessageResponse::ok() / err() ────────────────────────────────────

    #[test]
    fn send_message_response_ok_has_ret_zero_and_no_errmsg() {
        let r = SendMessageResponse::ok();
        assert_eq!(r.ret, Some(0));
        assert!(r.errmsg.is_none());
    }

    #[test]
    fn send_message_response_err_has_non_zero_ret_and_errmsg() {
        let r = SendMessageResponse::err(-1, "bad request");
        assert_eq!(r.ret, Some(-1));
        assert_eq!(r.errmsg.as_deref(), Some("bad request"));
    }
}

// ─── GetUpdates (getupdates endpoint) ────────────────────────────────────────

/// Request body for `POST /ilink/bot/getupdates`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GetUpdatesRequest {
    /// 客户端已确认的投递水位：把上次响应里的 `get_updates_buf` 原样回带即表示
    /// 「该批次已收到」。首次调用（或客户端丢失游标）为空串。
    #[serde(default)]
    pub get_updates_buf: String,
    /// 显式 ack 载体，与 `get_updates_buf` 同数空间；两者同时存在时取较大值。
    /// 上游 bridge 不会发送它，保留给能自行维护投递序号的客户端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ack_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_info: Option<BaseInfo>,
    /// Long-poll seconds (0 = return immediately if no messages). Defaults to 30 on Hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
}

impl GetUpdatesRequest {
    /// 客户端上送的 ack 水位：取 `get_updates_buf`（十进制）与 `last_ack_id`
    /// 的较大值。空串 / 非十进制 / 负数一律视为「未 ack」（`None`）。
    pub fn ack_watermark(&self) -> Option<u64> {
        let from_buf = self.get_updates_buf.trim().parse::<u64>().ok();
        let from_field = self.last_ack_id.and_then(|id| u64::try_from(id).ok());
        match (from_buf, from_field) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
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
    /// Hub 的投递高水位（恒非空）。客户端必须在下一次请求里原样回带它；只有被
    /// 回带过的批次才算确认，否则 Hub 会重复投递同一批消息（at-least-once）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_updates_buf: Option<String>,
}

// ─── SendMessage ─────────────────────────────────────────────────────────────

/// Request body for `POST /ilink/bot/sendmessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// 获取 CDN 上传地址的请求（透明转发到上游腾讯 iLink）。
///
/// 字段对齐 openclaw-weixin SDK 的 `GetUploadUrlReq`（该 SDK 经过实战验证）。
/// 注意：Hub 对该请求体做「反序列化 → 重新序列化 → 转发」，serde 会丢弃
/// 未声明的字段——所以这里的字段集必须与上游期望完全一致。
/// （2026-08 之前曾用臆造的 file_type/file_size/file_md5 schema，导致上游
/// 恒返回 ret=-2，已废弃。）
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct GetUploadUrlRequest {
    /// 随机 filekey（客户端生成，hex）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filekey: Option<String>,
    /// 媒体类型：1=image 2=video 3=file 4=voice
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_user_id: Option<String>,
    /// 原始文件字节数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rawsize: Option<u64>,
    /// 原始文件 MD5（hex）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rawfilemd5: Option<String>,
    /// AES-128-ECB 加密后密文大小
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesize: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_rawsize: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_rawfilemd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_filesize: Option<u64>,
    /// 图片可传 true 省略缩略图
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_need_thumb: Option<bool>,
    /// AES key 的 hex 字符串
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aeskey: Option<String>,
}

/// 获取 CDN 上传地址的响应（上游透传）。
///
/// 字段对齐 openclaw-weixin SDK 的 `GetUploadUrlResp`：
/// 优先用 `upload_full_url`（完整预签名 URL）；缺失时用 `upload_param`
/// 拼 CDN URL。错误时上游返回 ret/errmsg。
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct GetUploadUrlResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_upload_param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_full_url: Option<String>,
}

#[cfg(test)]
mod wire_roundtrip_tests {
    use super::*;

    /// 复现 hitl-server 发来的图片消息 JSON，验证 Hub 反序列化→再序列化后
    /// image_item.media（encrypt_query_param/aes_key/encrypt_type/mid_size）
    /// 完整保留、不丢字段。
    #[test]
    fn image_item_media_roundtrip_preserved() {
        let incoming = serde_json::json!({
            "context_token": "vctx_test",
            "to_user_id": "u@im.wechat",
            "from_user_id": "",
            "message_type": 2,
            "message_state": 2,
            "client_id": "hil-abc",
            "item_list": [{
                "type": 2,
                "image_item": {
                    "media": {
                        "encrypt_query_param": "pTn5z8BzIftZM_ACTFzglK_test_eqp",
                        "aes_key": "MzJjaGFyc0hleFRlc3RLZXk=",
                        "encrypt_type": 1
                    },
                    "mid_size": 9616
                }
            }]
        });
        let msg: WeixinMessage = serde_json::from_value(incoming.clone()).unwrap();
        let out = serde_json::to_value(&msg).unwrap();
        println!("ROUNDTRIP OUT: {out}");
        let img = &out["item_list"][0]["image_item"];
        assert_eq!(
            img["media"]["encrypt_query_param"],
            "pTn5z8BzIftZM_ACTFzglK_test_eqp"
        );
        assert_eq!(img["media"]["aes_key"], "MzJjaGFyc0hleFRlc3RLZXk=");
        assert_eq!(img["media"]["encrypt_type"], 1);
        assert_eq!(img["mid_size"], 9616);
        assert_eq!(out["item_list"][0]["type"], 2);
    }
}

#[cfg(test)]
mod get_updates_ack_tests {
    use super::*;

    fn req(buf: &str, last_ack_id: Option<i64>) -> GetUpdatesRequest {
        GetUpdatesRequest {
            get_updates_buf: buf.to_string(),
            last_ack_id,
            base_info: None,
            timeout: None,
        }
    }

    #[test]
    fn empty_cursor_and_no_field_means_no_ack() {
        assert_eq!(req("", None).ack_watermark(), None);
    }

    #[test]
    fn non_decimal_cursor_is_ignored() {
        assert_eq!(req("not-a-cursor", None).ack_watermark(), None);
    }

    #[test]
    fn non_negative_last_ack_id_is_used_when_cursor_is_empty() {
        assert_eq!(req("", Some(7)).ack_watermark(), Some(7));
    }

    #[test]
    fn negative_last_ack_id_is_ignored() {
        assert_eq!(req("", Some(-1)).ack_watermark(), None);
        assert_eq!(req("3", Some(-1)).ack_watermark(), Some(3));
    }

    #[test]
    fn larger_of_cursor_and_last_ack_id_wins() {
        assert_eq!(req("9", Some(3)).ack_watermark(), Some(9));
        assert_eq!(req("3", Some(9)).ack_watermark(), Some(9));
        assert_eq!(req("5", Some(5)).ack_watermark(), Some(5));
    }
}
