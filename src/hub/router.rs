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
    None
}

// ─── Router ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum RoutingDecision {
    ForwardTo(String),
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
            RoutingDecision::ForwardTo(vtoken.to_string())
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
            RoutingDecision::ForwardTo(ref v) if v == "default_vt"
        ));
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
