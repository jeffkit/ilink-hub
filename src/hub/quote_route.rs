//! Quote-aware routing: map iLink `ref_msg` / item `msg_id` back to the backend (or Hub)
//! that produced the quoted message, without requiring a short-id in the visible body.
//!
//! **Population**: when a downstream client (or Hub) calls `sendmessage`, we record a
//! pending entry keyed by outbound `client_id` (`ilink-hub:…`). If the real iLink
//! `getupdates` stream echoes that bot message (`message_type == 2`) with the same
//! `client_id`, we register `item.msg_id` (and top-level `message_id`) → origin.
//!
//! **Resolution**: inbound user messages that carry `ref_msg.message_item.msg_id`
//! hit the index first (unless the user message is an explicit `/…` hub command).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use super::router::{HubCommand, RoutingDecision};

/// Apply quote-based override when the user did not send an explicit hub `/…` command.
pub fn merge_routing_with_quote(
    base: RoutingDecision,
    quoted: Option<QuoteOrigin>,
) -> RoutingDecision {
    if matches!(&base, RoutingDecision::HubInternal(_)) {
        return base;
    }
    match quoted {
        Some(QuoteOrigin::Client {
            vtoken,
            session_name,
            ..
        }) => RoutingDecision::ForwardTo {
            vtoken,
            session_override: session_name,
        },
        Some(QuoteOrigin::Hub { cmd }) => RoutingDecision::HubInternal(cmd),
        None => base,
    }
}

/// Who should receive a follow-up when the user quote-replies.
#[derive(Debug, Clone)]
pub enum QuoteOrigin {
    /// A registered downstream client.
    Client {
        vtoken: String,
        name: String,
        label: Option<String>,
        /// The session that was active when this message was sent.
        session_name: Option<String>,
    },
    /// Hub-generated reply (e.g. `/list`); re-run the same hub action.
    Hub { cmd: HubCommand },
}

#[derive(Debug, Clone)]
struct PendingOutbound {
    origin: QuoteOrigin,
    deadline: Instant,
}

#[derive(Debug, Clone)]
struct IndexedOrigin {
    origin: QuoteOrigin,
    deadline: Instant,
}

/// In-memory index with TTL eviction (no persistence yet).
#[derive(Debug, Default)]
pub struct QuoteRouteIndex {
    pending_by_client_id: HashMap<String, PendingOutbound>,
    by_msg_key: HashMap<String, IndexedOrigin>,
}

const PENDING_TTL: Duration = Duration::from_secs(600);
const INDEX_TTL: Duration = Duration::from_secs(86400 * 7);

impl QuoteRouteIndex {
    /// After building the outbound `WeixinMessage` (with `ensure_outbound`), register
    /// so a later upstream echo can attach `msg_id` keys.
    pub fn register_pending_client(
        &mut self,
        client_id: &str,
        vtoken: String,
        name: String,
        label: Option<String>,
        session_name: Option<String>,
    ) {
        if client_id.is_empty() {
            return;
        }
        self.pending_by_client_id.insert(
            client_id.to_string(),
            PendingOutbound {
                origin: QuoteOrigin::Client {
                    vtoken,
                    name,
                    label,
                    session_name,
                },
                deadline: Instant::now() + PENDING_TTL,
            },
        );
    }

    pub fn register_pending_hub(&mut self, client_id: &str, cmd: HubCommand) {
        if client_id.is_empty() {
            return;
        }
        self.pending_by_client_id.insert(
            client_id.to_string(),
            PendingOutbound {
                origin: QuoteOrigin::Hub { cmd },
                deadline: Instant::now() + PENDING_TTL,
            },
        );
    }

    /// Call for upstream messages that look like bot-side copies (`message_type == 2`).
    pub fn observe_upstream_bot_message(&mut self, msg: &crate::ilink::types::WeixinMessage) {
        let client_id = match msg.client_id.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };
        // Diagnostic: log the echo structure so we can verify create_time_ms alignment with ref_msg.
        {
            let top_create_ms = msg.create_time_ms;
            let top_message_id = msg.message_id;
            let item_fields: Vec<_> = msg.item_list.as_deref().unwrap_or(&[]).iter().map(|item| {
                let msg_id = item.extra.get("msg_id").and_then(|v| v.as_str()).unwrap_or("(none)").to_string();
                let create_ms = item.extra.get("create_time_ms").and_then(|v| v.as_i64());
                (msg_id, create_ms)
            }).collect();
            tracing::debug!(
                client_id,
                top_message_id,
                top_create_ms,
                ?item_fields,
                "bot echo observed"
            );
        }
        let Some(pending) = self.pending_by_client_id.remove(client_id) else {
            return;
        };
        if Instant::now() > pending.deadline {
            return;
        }
        let origin = pending.origin;
        if let Some(mid) = msg.message_id {
            self.insert_key(format!("m:{mid}"), origin.clone());
        }
        if let Some(items) = &msg.item_list {
            for item in items {
                if let Some(id) = item_msg_id(item) {
                    self.insert_key(format!("i:{id}"), origin.clone());
                }
            }
        }
    }

    fn insert_key(&mut self, key: String, origin: QuoteOrigin) {
        self.by_msg_key.insert(
            key,
            IndexedOrigin {
                origin,
                deadline: Instant::now() + INDEX_TTL,
            },
        );
    }

    /// If the user quote-replies, resolve the quoted bot item to a [`QuoteOrigin`].
    pub fn resolve_user_quote(
        &mut self,
        msg: &crate::ilink::types::WeixinMessage,
    ) -> Option<QuoteOrigin> {
        for key in collect_quoted_msg_keys(msg) {
            if let Some(entry) = self.by_msg_key.get(&key) {
                if Instant::now() <= entry.deadline {
                    return Some(entry.origin.clone());
                }
            }
        }
        None
    }

    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        self.pending_by_client_id.retain(|_, p| now <= p.deadline);
        self.by_msg_key.retain(|_, v| now <= v.deadline);
    }
}

fn item_msg_id(item: &crate::ilink::types::MessageItem) -> Option<String> {
    extra_str(&item.extra, &["msg_id"])
}

/// Pull quoted message item ids from the first text-like item's `ref_msg`.
fn collect_quoted_msg_keys(msg: &crate::ilink::types::WeixinMessage) -> Vec<String> {
    let mut out = Vec::new();
    let Some(items) = &msg.item_list else {
        return out;
    };
    for item in items {
        let Some(extra) = item.extra.as_object() else {
            continue;
        };
        let Some(ref_msg) = extra.get("ref_msg") else {
            continue;
        };
        let Some(mi) = ref_msg.get("message_item") else {
            continue;
        };
        if let Some(id) = json_str(mi.get("msg_id")) {
            out.push(format!("i:{id}"));
        }
        if let Some(mid) = mi.get("message_id").and_then(|v| v.as_i64()) {
            out.push(format!("m:{mid}"));
        }
        if let Some(Json::Object(map)) = mi.get("extra") {
            if let Some(id) = map.get("msg_id").and_then(|v| v.as_str()) {
                out.push(format!("i:{id}"));
            }
        }
    }
    out
}

fn extra_str(extra: &Json, path: &[&str]) -> Option<String> {
    let mut cur = extra;
    for p in path {
        cur = cur.get(*p)?;
    }
    json_str(Some(cur))
}

fn json_str(v: Option<&Json>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(n) = v.as_i64() {
        return Some(n.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem, WeixinMessage};

    #[test]
    fn collect_keys_from_sample_ref() {
        let extra: Json = serde_json::from_str(
            r#"{
            "ref_msg": {
                "message_item": {
                    "msg_id": "v1:quoted-bot-item",
                    "message_id": 999888777,
                    "type": 1,
                    "text_item": { "text": "hello" }
                }
            }
        }"#,
        )
        .unwrap();
        let msg = WeixinMessage {
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("再来一次".into()),
                }),
                extra,
                voice_item: None,
            }]),
            ..Default::default()
        };
        let keys = collect_quoted_msg_keys(&msg);
        assert!(keys.contains(&"i:v1:quoted-bot-item".to_string()));
        assert!(keys.contains(&"m:999888777".to_string()));
    }

    #[test]
    fn merge_quote_overrides_forward() {
        let base = RoutingDecision::ForwardTo {
            vtoken: "default_vt".into(),
            session_override: None,
        };
        let q = QuoteOrigin::Client {
            vtoken: "quoted_vt".into(),
            name: "n".into(),
            label: None,
            session_name: Some("feature-a".into()),
        };
        let out = merge_routing_with_quote(base, Some(q));
        assert!(matches!(
            out,
            RoutingDecision::ForwardTo { ref vtoken, ref session_override }
                if vtoken == "quoted_vt" && session_override.as_deref() == Some("feature-a")
        ));
    }

    #[test]
    fn merge_quote_overrides_broadcast() {
        let out = merge_routing_with_quote(
            RoutingDecision::Broadcast,
            Some(QuoteOrigin::Client {
                vtoken: "vt".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            }),
        );
        assert!(matches!(out, RoutingDecision::ForwardTo { ref vtoken, .. } if vtoken == "vt"));
    }

    #[test]
    fn merge_hub_internal_from_quote() {
        let out = merge_routing_with_quote(
            RoutingDecision::ForwardTo {
                vtoken: "x".into(),
                session_override: None,
            },
            Some(QuoteOrigin::Hub {
                cmd: HubCommand::List,
            }),
        );
        assert!(matches!(
            out,
            RoutingDecision::HubInternal(HubCommand::List)
        ));
    }

    #[test]
    fn merge_explicit_hub_command_not_overridden_by_quote() {
        let base = RoutingDecision::HubInternal(HubCommand::Status);
        let out = merge_routing_with_quote(
            base,
            Some(QuoteOrigin::Client {
                vtoken: "vt".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            }),
        );
        assert!(matches!(
            out,
            RoutingDecision::HubInternal(HubCommand::Status)
        ));
    }

    #[test]
    fn merge_no_quote_keeps_forward() {
        let base = RoutingDecision::ForwardTo {
            vtoken: "keep".into(),
            session_override: None,
        };
        let out = merge_routing_with_quote(base, None);
        assert!(matches!(out, RoutingDecision::ForwardTo { ref vtoken, .. } if vtoken == "keep"));
    }

    #[test]
    fn observe_unknown_client_id_never_indexes() {
        let mut idx = QuoteRouteIndex::default();
        let echo = WeixinMessage {
            message_type: Some(2),
            client_id: Some("orphan".into()),
            message_id: Some(777),
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("b".into()),
                }),
                extra: serde_json::json!({ "msg_id": "v1:orphan" }),
                voice_item: None,
            }]),
            ..Default::default()
        };
        idx.observe_upstream_bot_message(&echo);
        let user = WeixinMessage {
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("u".into()),
                }),
                extra: serde_json::json!({
                    "ref_msg": { "message_item": { "msg_id": "v1:orphan" } }
                }),
                voice_item: None,
            }]),
            ..Default::default()
        };
        assert!(idx.resolve_user_quote(&user).is_none());
    }

    #[test]
    fn resolve_without_ref_returns_none() {
        let mut idx = QuoteRouteIndex::default();
        idx.register_pending_client("c1", "vt".into(), "n".into(), None, None);
        let user = WeixinMessage {
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hi".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }]),
            ..Default::default()
        };
        assert!(idx.resolve_user_quote(&user).is_none());
    }

    #[test]
    fn observe_then_resolve() {
        let mut idx = QuoteRouteIndex::default();
        idx.register_pending_client(
            "ilink-hub:test-client-id",
            "vhub_abc".into(),
            "echo".into(),
            Some("echo test".into()),
            Some("feature-a".into()),
        );
        let echo = WeixinMessage {
            message_type: Some(2),
            client_id: Some("ilink-hub:test-client-id".into()),
            message_id: Some(42),
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("bot said".into()),
                }),
                extra: serde_json::json!({ "msg_id": "v1:item-1" }),
                voice_item: None,
            }]),
            ..Default::default()
        };
        idx.observe_upstream_bot_message(&echo);
        let user = WeixinMessage {
            message_type: Some(1),
            from_user_id: Some("user@x".into()),
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("again".into()),
                }),
                extra: serde_json::json!({
                    "ref_msg": {
                        "message_item": {
                            "msg_id": "v1:item-1"
                        }
                    }
                }),
                voice_item: None,
            }]),
            ..Default::default()
        };
        let origin = idx.resolve_user_quote(&user).expect("resolve");
        match origin {
            QuoteOrigin::Client {
                vtoken,
                name,
                session_name,
                ..
            } => {
                assert_eq!(vtoken, "vhub_abc");
                assert_eq!(name, "echo");
                assert_eq!(session_name.as_deref(), Some("feature-a"));
            }
            QuoteOrigin::Hub { .. } => panic!("expected client"),
        }
    }
}
