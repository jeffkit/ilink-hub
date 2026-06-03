/// Message router — decides which backend client receives each inbound message.
/// Routing state is per-WeChat-user (from_user field).

use std::collections::HashMap;
use tracing::debug;

use crate::ilink::types::InboundMessage;

// ─── Hub commands ────────────────────────────────────────────────────────────

/// Commands the WeChat user can send to control routing.
#[derive(Debug, PartialEq)]
pub enum HubCommand {
    List,
    UseClient(String),
    Broadcast(String),
    Status,
}

pub fn parse_hub_command(text: &str) -> Option<HubCommand> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("/list") || text.eq_ignore_ascii_case("/ls") {
        return Some(HubCommand::List);
    }
    if text.eq_ignore_ascii_case("/status") {
        return Some(HubCommand::Status);
    }
    if let Some(rest) = text.strip_prefix("/use ").or_else(|| text.strip_prefix("/switch ")) {
        return Some(HubCommand::UseClient(rest.trim().to_string()));
    }
    if let Some(rest) = text.strip_prefix("/broadcast ").or_else(|| text.strip_prefix("/all ")) {
        return Some(HubCommand::Broadcast(rest.trim().to_string()));
    }
    None
}

// ─── Router ──────────────────────────────────────────────────────────────────

/// Routing decision for an inbound message.
#[derive(Debug)]
pub enum RoutingDecision {
    /// Route to a specific client vtoken
    ForwardTo(String),
    /// Broadcast to all online clients
    Broadcast,
    /// Handle locally (hub command like /list, /use)
    HubInternal(HubCommand),
}

pub struct Router {
    /// from_user → active client vtoken
    active_routes: HashMap<String, String>,
    /// Default client vtoken used for users with no active route set
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

    pub fn set_route(&mut self, from_user: &str, vtoken: String) {
        self.active_routes.insert(from_user.to_string(), vtoken);
    }

    pub fn get_route(&self, from_user: &str) -> Option<&str> {
        self.active_routes
            .get(from_user)
            .map(String::as_str)
            .or(self.default_client.as_deref())
    }

    /// Decide routing for an inbound message.
    pub fn route(&self, msg: &InboundMessage) -> RoutingDecision {
        // Check for hub commands first
        if let Some(text) = &msg.content {
            if let Some(cmd) = parse_hub_command(text) {
                return RoutingDecision::HubInternal(cmd);
            }
        }

        // Route based on active selection for this user
        if let Some(vtoken) = self.get_route(&msg.from_user) {
            debug!(from_user = %msg.from_user, vtoken = %vtoken, "routing message");
            RoutingDecision::ForwardTo(vtoken.to_string())
        } else {
            // No route set and no default — broadcast to all online
            RoutingDecision::Broadcast
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn no_command() {
        assert_eq!(parse_hub_command("hello world"), None);
    }
}
