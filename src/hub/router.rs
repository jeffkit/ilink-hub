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
    if text.eq_ignore_ascii_case("/status") {
        return Some(HubCommand::Status);
    }
    if text.eq_ignore_ascii_case("/help") || text.eq_ignore_ascii_case("/?") {
        return Some(HubCommand::Help);
    }
    if let Some(rest) = text
        .strip_prefix("/use ")
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

    // /session subcommands
    if text.eq_ignore_ascii_case("/session list") || text.eq_ignore_ascii_case("/session ls") {
        return Some(HubCommand::SessionList);
    }
    if let Some(rest) = text.strip_prefix("/session new ").or_else(|| {
        if text.eq_ignore_ascii_case("/session new") {
            Some("")
        } else {
            None
        }
    }) {
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
    if let Some(rest) = text.strip_prefix("/session use ") {
        let name = rest.trim().to_string();
        if !name.is_empty() {
            return Some(HubCommand::SessionUse(name));
        }
    }
    if let Some(rest) = text
        .strip_prefix("/session delete ")
        .or_else(|| text.strip_prefix("/session rm "))
        .or_else(|| text.strip_prefix("/session del "))
    {
        let name = rest.trim().to_string();
        if !name.is_empty() {
            return Some(HubCommand::SessionDelete(name));
        }
    }

    None
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
            debug!(from_user_id, vtoken, "routing message");
            RoutingDecision::ForwardTo { vtoken: vtoken.to_string(), session_override: None }
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
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
            }]),
            ..Default::default()
        };
        assert!(matches!(
            r.route(&msg),
            RoutingDecision::ForwardTo { ref vtoken, .. } if vtoken == "default_vt"
        ));
    }

    #[test]
    fn parse_session_list_command() {
        assert_eq!(parse_hub_command("/session list"), Some(HubCommand::SessionList));
        assert_eq!(parse_hub_command("/session ls"), Some(HubCommand::SessionList));
    }

    #[test]
    fn parse_session_new_command() {
        assert_eq!(
            parse_hub_command("/session new feature-a"),
            Some(HubCommand::SessionNew("feature-a".to_string(), "".to_string()))
        );
        assert_eq!(
            parse_hub_command("/session new feature-b some-uuid-123"),
            Some(HubCommand::SessionNew("feature-b".to_string(), "some-uuid-123".to_string()))
        );
        // bare /session new → name is a timestamp-based unique name like "session-20260609-123456"
        if let Some(HubCommand::SessionNew(name, uuid)) = parse_hub_command("/session new") {
            assert!(name.starts_with("session-"), "expected timestamp name, got: {name}");
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
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
            }]),
            ..Default::default()
        };
        assert!(matches!(r.route(&msg), RoutingDecision::Broadcast));
    }
}
