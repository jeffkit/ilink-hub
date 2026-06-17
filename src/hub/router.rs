//! Message router — decides which backend client receives each inbound message.
//! Routing state is per-WeChat-user (from_user_id field).

use std::collections::HashMap;
use tracing::debug;

use crate::ilink::types::WeixinMessage;

// ─── Hub commands ────────────────────────────────────────────────────────────

/// Commands the WeChat user can send to control routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubCommand {
    List,
    UseClient(String),
    Broadcast(String),
    Status,
    Help,
    /// List all backend sessions for the current conversation.
    SessionList,
    /// Create a new named backend session (optionally with an initial UUID).
    /// `(session_name, initial_uuid)` — uuid is empty string if not provided.
    SessionNew(String, String),
    /// Switch the active backend session.
    SessionUse(String),
    /// Delete a named backend session.
    SessionDelete(String),
}

pub fn parse_hub_command(text: &str) -> Option<HubCommand> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("/list") || text.eq_ignore_ascii_case("/ls") {
        return Some(HubCommand::List);
    }
    if text.eq_ignore_ascii_case("/status") || text.eq_ignore_ascii_case("/s") {
        return Some(HubCommand::Status);
    }
    if text.eq_ignore_ascii_case("/help")
        || text.eq_ignore_ascii_case("/?")
        || text.eq_ignore_ascii_case("/h")
    {
        return Some(HubCommand::Help);
    }
    if let Some(rest) = text
        .strip_prefix("/use ")
        .or_else(|| text.strip_prefix("/u "))
        .or_else(|| text.strip_prefix("/switch "))
    {
        return Some(HubCommand::UseClient(rest.trim().to_string()));
    }
    if let Some(rest) = text
        .strip_prefix("/broadcast ")
        .or_else(|| text.strip_prefix("/all "))
    {
        return Some(HubCommand::Broadcast(rest.trim().to_string()));
    }

    // /session subcommands — full forms and short aliases (/sl /sn /su /sd)
    if text.eq_ignore_ascii_case("/session list")
        || text.eq_ignore_ascii_case("/session ls")
        || text.eq_ignore_ascii_case("/sl")
    {
        return Some(HubCommand::SessionList);
    }
    if let Some(rest) = text
        .strip_prefix("/session new ")
        .or_else(|| text.strip_prefix("/sn "))
        .or_else(|| {
            if text.eq_ignore_ascii_case("/session new") || text.eq_ignore_ascii_case("/sn") {
                Some("")
            } else {
                None
            }
        })
    {
        let rest = rest.trim();
        let mut parts = rest.splitn(2, ' ');
        let name = parts.next().unwrap_or("").trim().to_string();
        let uuid = parts.next().unwrap_or("").trim().to_string();
        let name = if name.is_empty() {
            // 无参数时生成带时间戳的唯一名称，避免覆盖已有的 "default" session
            let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
            format!("session-{ts}")
        } else {
            name
        };
        return Some(HubCommand::SessionNew(name, uuid));
    }
    if let Some(rest) = text
        .strip_prefix("/session use ")
        .or_else(|| text.strip_prefix("/su "))
    {
        let name = rest.trim().to_string();
        if !name.is_empty() {
            return Some(HubCommand::SessionUse(name));
        }
    }
    if let Some(rest) = text
        .strip_prefix("/session delete ")
        .or_else(|| text.strip_prefix("/session rm "))
        .or_else(|| text.strip_prefix("/session del "))
        .or_else(|| text.strip_prefix("/sd "))
    {
        let name = rest.trim().to_string();
        if !name.is_empty() {
            return Some(HubCommand::SessionDelete(name));
        }
    }

    None
}

/// Parse an `@<backend> <message>` mention (a temporary shortcut, analogous to a quote-reply).
///
/// The backend **name** is everything between the leading `@` and the first whitespace or the
/// first non-ASCII character; the **message** is the remainder (trimmed). Backend names are
/// ASCII-only (letters, digits, hyphens, underscores), so a non-ASCII character (e.g. Chinese)
/// immediately following the name is treated as the start of the message — this allows users to
/// write `@claude你好` without an explicit space between the name and the message body. The name
/// is the same identifier used by `/use <name>` (resolved via the client registry by the caller).
///
/// Returns `None` when the text does not start with `@` or the name is empty. This parser does
/// **not** validate that the name is a registered backend — that check happens in the dispatcher
/// where the registry is available; an unknown name falls back to a normal message.
pub fn parse_at_mention(text: &str) -> Option<(String, String)> {
    let rest = text.trim_start().strip_prefix('@')?;
    // Collect ASCII word characters (letters, digits, hyphens, underscores) as the name.
    // The name ends at the first whitespace or the first non-ASCII character, whichever comes
    // first — this lets users omit the space between an ASCII backend name and a non-ASCII
    // message body (e.g. `@claude你好` is equivalent to `@claude 你好`).
    // When the text starts with a non-ASCII character (e.g. `@后端 消息`), we fall back to
    // splitting on whitespace so purely non-ASCII backend names still work.
    let starts_with_ascii = rest.chars().next().map(|c| c.is_ascii()).unwrap_or(false);
    let (name, message) = if starts_with_ascii {
        let split_at = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || !c.is_ascii())
            .map(|(i, _)| i);
        match split_at {
            Some(i) => (&rest[..i], rest[i..].trim_start()),
            None => (rest, ""),
        }
    } else {
        match rest.split_once(char::is_whitespace) {
            Some((n, m)) => (n, m),
            None => (rest, ""),
        }
    };
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), message.trim().to_string()))
}

// ─── Router ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum RoutingDecision {
    /// Route to a specific backend vtoken, optionally pinning to a specific session.
    ForwardTo {
        vtoken: String,
        /// When set (from a quote-reply), override the active session lookup with this session.
        session_override: Option<String>,
    },
    Broadcast,
    HubInternal(HubCommand),
}

pub struct Router {
    active_routes: HashMap<String, String>,
    default_client: Option<String>,
}

impl Router {
    pub fn new(default_client: Option<String>) -> Self {
        Self {
            active_routes: HashMap::new(),
            default_client,
        }
    }

    pub fn set_default(&mut self, vtoken: String) {
        self.default_client = Some(vtoken);
    }

    /// Clear the default route so subsequent messages fall through to
    /// broadcast (used by tests that want to exercise the fan-out path
    /// without first deleting the default client).
    pub fn unset_default(&mut self) {
        self.default_client = None;
    }

    pub fn set_route(&mut self, from_user_id: &str, vtoken: String) {
        self.active_routes.insert(from_user_id.to_string(), vtoken);
    }

    /// Drop per-user routes and default client entry for a removed backend vtoken.
    pub fn remove_routes_for_vtoken(&mut self, vtoken: &str, new_default: Option<String>) {
        if self.default_client.as_deref() == Some(vtoken) {
            self.default_client = new_default;
        }
        self.active_routes.retain(|_, vt| vt != vtoken);
    }

    pub fn get_route(&self, from_user_id: &str) -> Option<&str> {
        self.active_routes
            .get(from_user_id)
            .map(String::as_str)
            .or(self.default_client.as_deref())
    }

    /// Decide routing for an inbound message.
    pub fn route(&self, msg: &WeixinMessage) -> RoutingDecision {
        // Check for hub commands in text messages
        if let Some(text) = msg.text() {
            if let Some(cmd) = parse_hub_command(text) {
                return RoutingDecision::HubInternal(cmd);
            }
        }

        let from_user_id = msg.from_user_id.as_deref().unwrap_or_default();
        if let Some(vtoken) = self.get_route(from_user_id) {
            debug!(
                from_user_id,
                vtoken = %crate::redact_token(vtoken),
                "routing message"
            );
            RoutingDecision::ForwardTo {
                vtoken: vtoken.to_string(),
                session_override: None,
            }
        } else {
            RoutingDecision::Broadcast
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem, WeixinMessage};

    #[test]
    fn parse_list_command() {
        assert_eq!(parse_hub_command("/list"), Some(HubCommand::List));
        assert_eq!(parse_hub_command("/ls"), Some(HubCommand::List));
    }

    #[test]
    fn parse_use_command() {
        assert_eq!(
            parse_hub_command("/use mac-workspace"),
            Some(HubCommand::UseClient("mac-workspace".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_basic() {
        assert_eq!(
            parse_at_mention("@mac-workspace 帮我看下日志"),
            Some(("mac-workspace".to_string(), "帮我看下日志".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_name_only_no_message() {
        assert_eq!(
            parse_at_mention("@claude"),
            Some(("claude".to_string(), "".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_first_space_terminates_name() {
        // The name is everything before the first space; the rest is the message,
        // even when the message itself contains spaces.
        assert_eq!(
            parse_at_mention("@后端 这条 消息 有 空格"),
            Some(("后端".to_string(), "这条 消息 有 空格".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_no_space_before_non_ascii_message() {
        // Non-ASCII character immediately following the name acts as a split point.
        // Users can type @claude你好 without an explicit space.
        assert_eq!(
            parse_at_mention("@claude你好"),
            Some(("claude".to_string(), "你好".to_string()))
        );
        assert_eq!(
            parse_at_mention("@backend-name消息内容"),
            Some(("backend-name".to_string(), "消息内容".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_trims_leading_whitespace() {
        assert_eq!(
            parse_at_mention("   @bot hi"),
            Some(("bot".to_string(), "hi".to_string()))
        );
    }

    #[test]
    fn parse_at_mention_rejects_non_at_and_empty_name() {
        assert_eq!(parse_at_mention("hello @bot"), None);
        assert_eq!(parse_at_mention("@ no name"), None);
        assert_eq!(parse_at_mention("@"), None);
    }

    #[test]
    fn parse_broadcast_command() {
        assert_eq!(
            parse_hub_command("/broadcast hello"),
            Some(HubCommand::Broadcast("hello".to_string()))
        );
    }

    #[test]
    fn route_uses_default_client_when_no_per_user_route() {
        let r = Router::new(Some("default_vt".into()));
        let msg = WeixinMessage {
            from_user_id: Some("user@wechat".into()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }])),
            ..Default::default()
        };
        assert!(matches!(
            r.route(&msg),
            RoutingDecision::ForwardTo { ref vtoken, .. } if vtoken == "default_vt"
        ));
    }

    #[test]
    fn parse_session_list_command() {
        assert_eq!(
            parse_hub_command("/session list"),
            Some(HubCommand::SessionList)
        );
        assert_eq!(
            parse_hub_command("/session ls"),
            Some(HubCommand::SessionList)
        );
    }

    #[test]
    fn parse_session_new_command() {
        assert_eq!(
            parse_hub_command("/session new feature-a"),
            Some(HubCommand::SessionNew(
                "feature-a".to_string(),
                "".to_string()
            ))
        );
        assert_eq!(
            parse_hub_command("/session new feature-b some-uuid-123"),
            Some(HubCommand::SessionNew(
                "feature-b".to_string(),
                "some-uuid-123".to_string()
            ))
        );
        // bare /session new → name is a timestamp-based unique name like "session-20260609-123456"
        if let Some(HubCommand::SessionNew(name, uuid)) = parse_hub_command("/session new") {
            assert!(
                name.starts_with("session-"),
                "expected timestamp name, got: {name}"
            );
            assert_eq!(uuid, "");
        } else {
            panic!("/session new should parse as SessionNew");
        }
    }

    #[test]
    fn parse_session_use_command() {
        assert_eq!(
            parse_hub_command("/session use my-session"),
            Some(HubCommand::SessionUse("my-session".to_string()))
        );
    }

    #[test]
    fn parse_session_delete_command() {
        assert_eq!(
            parse_hub_command("/session delete old-session"),
            Some(HubCommand::SessionDelete("old-session".to_string()))
        );
        assert_eq!(
            parse_hub_command("/session rm old-session"),
            Some(HubCommand::SessionDelete("old-session".to_string()))
        );
        assert_eq!(
            parse_hub_command("/session del old-session"),
            Some(HubCommand::SessionDelete("old-session".to_string()))
        );
    }

    #[test]
    fn route_broadcast_when_no_default_and_no_route() {
        let r = Router::new(None);
        let msg = WeixinMessage {
            from_user_id: Some("user@wechat".into()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }])),
            ..Default::default()
        };
        assert!(matches!(r.route(&msg), RoutingDecision::Broadcast));
    }

    #[test]
    fn route_redacts_vtoken_in_logs() {
        use tracing::field::{Field, Visit};
        use tracing::{Event, Metadata, Subscriber};

        struct MockSubscriber {
            last_vtoken: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        }

        impl Subscriber for MockSubscriber {
            fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }
            fn event(&self, event: &Event<'_>) {
                struct VTokenVisitor {
                    vtoken: Option<String>,
                }
                impl Visit for VTokenVisitor {
                    fn record_str(&mut self, field: &Field, value: &str) {
                        if field.name() == "vtoken" {
                            self.vtoken = Some(value.to_string());
                        }
                    }
                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        if field.name() == "vtoken" {
                            self.vtoken = Some(format!("{:?}", value));
                        }
                    }
                }
                let mut visitor = VTokenVisitor { vtoken: None };
                event.record(&mut visitor);
                if let Some(vt) = visitor.vtoken {
                    if let Ok(mut guard) = self.last_vtoken.lock() {
                        *guard = Some(vt);
                    }
                }
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        let logged_vtoken = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sub = MockSubscriber {
            last_vtoken: logged_vtoken.clone(),
        };

        let dispatcher = tracing::Dispatch::new(sub);
        tracing::dispatcher::with_default(&dispatcher, || {
            let r = Router::new(Some("very_long_vtoken_that_should_be_redacted".into()));
            let msg = WeixinMessage {
                from_user_id: Some("user@wechat".into()),
                item_list: Some(std::sync::Arc::new(vec![MessageItem {
                    item_type: Some(1),
                    text_item: Some(TextItem {
                        text: Some("hello".into()),
                    }),
                    extra: serde_json::Value::Object(Default::default()),
                    voice_item: None,
                }])),
                ..Default::default()
            };
            let decision = r.route(&msg);
            assert!(matches!(
                decision,
                RoutingDecision::ForwardTo { ref vtoken, .. } if vtoken == "very_long_vtoken_that_should_be_redacted"
            ));
        });

        let guard = logged_vtoken.lock().unwrap();
        let redacted = guard.as_ref().expect("expected vtoken to be logged");
        assert_eq!(redacted, "very_lon…");
    }
}
