//! Quote-aware routing: map a WeChat quote-reply back to the backend (or Hub) that
//! produced the quoted message.
//!
//! **Why content-based?** The real iLink `getupdates` stream does **not** echo bot
//! messages back, and the `ref_msg.message_item` carried by a user's quote-reply contains
//! **no `msg_id` / `message_id`** — only the quoted text plus second-granularity
//! timestamps. So any `msg_id`-based correlation can never fire in practice. We instead
//! index each outbound message by its exact rendered text (which WeChat reproduces verbatim
//! inside `ref_msg`, including our `— workspace · session` footer) and use the original send
//! time to disambiguate when several backends/sessions sent identical text.
//!
//! **Scoping:** entries are scoped per conversation (the WeChat sender / `from_user_id`, or
//! group id) so a quote-reply in one conversation can never resolve to a message the Hub
//! sent into a *different* conversation.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// An outbound message indexed by its rendered text content (see module docs).
#[derive(Debug, Clone)]
struct ContentEntry {
    /// Conversation scope (`from_user_id` / group key) this message was sent into.
    scope: String,
    origin: QuoteOrigin,
    /// Approximate epoch-ms when the Hub sent this message (tiebreaker against the quoted
    /// `ref_msg.create_time_ms`).
    created_ms: i64,
    deadline: Instant,
}

/// In-memory index with TTL eviction (no persistence yet).
#[derive(Debug, Default)]
pub struct QuoteRouteIndex {
    /// Content signature → outbound origins (scoped per conversation).
    by_content: HashMap<String, Vec<ContentEntry>>,
}

const INDEX_TTL: Duration = Duration::from_secs(86400 * 7);
/// Cap entries per content signature to bound memory when the same text is sent repeatedly.
const MAX_CONTENT_ENTRIES_PER_KEY: usize = 32;
/// Length (in chars) of the prefix key used to tolerate WeChat truncating long quoted text.
const CONTENT_PREFIX_CHARS: usize = 48;

impl QuoteRouteIndex {
    /// Index an outbound message by its exact rendered text so a later quote-reply in the
    /// same conversation routes back to its origin.
    ///
    /// * `scope` — conversation key (the WeChat `from_user_id`, or group key).
    /// * `text` — the final body actually sent to iLink (after any origin footer), since that
    ///   is exactly what WeChat reproduces inside `ref_msg`.
    pub fn register_outbound_content(&mut self, scope: &str, text: &str, origin: QuoteOrigin) {
        let now_ms = now_millis();
        let deadline = Instant::now() + INDEX_TTL;
        for key in content_keys(text) {
            let bucket = self.by_content.entry(key).or_default();
            bucket.push(ContentEntry {
                scope: scope.to_string(),
                origin: origin.clone(),
                created_ms: now_ms,
                deadline,
            });
            if bucket.len() > MAX_CONTENT_ENTRIES_PER_KEY {
                let overflow = bucket.len() - MAX_CONTENT_ENTRIES_PER_KEY;
                bucket.drain(0..overflow);
            }
        }
    }

    /// Resolve a quoted text (+ optional quoted timestamp) within `scope` to an outbound
    /// origin. When multiple origins share the same text, the one whose send time is closest
    /// to the quoted `create_time_ms` wins; otherwise the most recently sent one wins.
    fn resolve_by_content(
        &self,
        scope: &str,
        text: &str,
        ref_ms: Option<i64>,
    ) -> Option<QuoteOrigin> {
        let now = Instant::now();
        for key in content_keys(text) {
            let Some(bucket) = self.by_content.get(&key) else {
                continue;
            };
            let best = bucket
                .iter()
                .filter(|e| now <= e.deadline && e.scope == scope)
                .min_by_key(|e| match ref_ms {
                    Some(ms) => (e.created_ms - ms).abs(),
                    // No quoted timestamp: prefer the most recent send (smallest negative age).
                    None => -e.created_ms,
                });
            if let Some(entry) = best {
                return Some(entry.origin.clone());
            }
        }
        None
    }

    /// If the user quote-replies, resolve the quoted message to a [`QuoteOrigin`].
    /// `scope` is the inbound message's conversation key (`from_user_id` / group key).
    pub fn resolve_user_quote(
        &mut self,
        scope: &str,
        msg: &crate::ilink::types::WeixinMessage,
    ) -> Option<QuoteOrigin> {
        let (text, ref_ms) = collect_quoted_content(msg)?;
        self.resolve_by_content(scope, &text, ref_ms)
    }

    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        for bucket in self.by_content.values_mut() {
            bucket.retain(|e| now <= e.deadline);
        }
        self.by_content.retain(|_, bucket| !bucket.is_empty());
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalized signature of a message body. WeChat reproduces the quoted text verbatim
/// (including our `\n\n— workspace · session` footer), so a trimmed exact match is reliable.
fn content_sig(text: &str) -> String {
    text.trim().to_string()
}

/// Keys under which an outbound text is indexed (and looked up): the full trimmed body, plus
/// a leading-character prefix so a long body that WeChat truncates inside `ref_msg` still
/// matches.
fn content_keys(text: &str) -> Vec<String> {
    let sig = content_sig(text);
    if sig.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![format!("full:{sig}")];
    let prefix: String = sig.chars().take(CONTENT_PREFIX_CHARS).collect();
    if prefix.len() < sig.len() {
        keys.push(format!("pre:{prefix}"));
    }
    keys
}

/// Pull the quoted text and its (second-granularity) `create_time_ms` from a user message's
/// `ref_msg`, used for content-based quote routing.
fn collect_quoted_content(
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<(String, Option<i64>)> {
    let items = msg.item_list.as_ref()?;
    for item in items.iter() {
        let Some(extra) = item.extra.as_object() else {
            continue;
        };
        let Some(mi) = extra.get("ref_msg").and_then(|r| r.get("message_item")) else {
            continue;
        };
        let text = mi
            .get("text_item")
            .and_then(|t| t.get("text"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty());
        if let Some(text) = text {
            let ref_ms = mi.get("create_time_ms").and_then(|v| v.as_i64());
            return Some((text.to_string(), ref_ms));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem, WeixinMessage};

    const SCOPE: &str = "user@x";

    fn quote_reply(scope_text: &str, quoted_text: &str, ref_ms: Option<i64>) -> WeixinMessage {
        let mut ref_item = serde_json::json!({
            "ref_msg": {
                "message_item": {
                    "type": 1,
                    "text_item": { "text": quoted_text }
                }
            }
        });
        if let Some(ms) = ref_ms {
            ref_item["ref_msg"]["message_item"]["create_time_ms"] = serde_json::Value::from(ms);
        }
        WeixinMessage {
            message_type: Some(1),
            from_user_id: Some(SCOPE.into()),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some(scope_text.into()),
                }),
                extra: ref_item,
                voice_item: None,
            }])),
            ..Default::default()
        }
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
    fn resolve_without_ref_returns_none() {
        let mut idx = QuoteRouteIndex::default();
        idx.register_outbound_content(
            SCOPE,
            "hi",
            QuoteOrigin::Client {
                vtoken: "vt".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            },
        );
        let user = WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hi".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }])),
            ..Default::default()
        };
        assert!(idx.resolve_user_quote(SCOPE, &user).is_none());
    }

    /// Real iLink quote-reply: `ref_msg.message_item` has NO `msg_id`/`message_id`, only the
    /// quoted text plus second-granularity timestamps. Content indexing must still resolve it.
    #[test]
    fn resolve_by_content_when_ref_has_no_msg_id() {
        let mut idx = QuoteRouteIndex::default();
        let sent = "你好！有什么我可以帮你的吗？\n\n— ilink-claude · session-20260611-125634";
        idx.register_outbound_content(
            SCOPE,
            sent,
            QuoteOrigin::Client {
                vtoken: "vhub_867".into(),
                name: "ilink-claude".into(),
                label: None,
                session_name: Some("session-20260611-125634".into()),
            },
        );

        let user = quote_reply("你有什么工具", sent, Some(1781153810000));
        match idx
            .resolve_user_quote(SCOPE, &user)
            .expect("content resolve")
        {
            QuoteOrigin::Client {
                vtoken,
                session_name,
                ..
            } => {
                assert_eq!(vtoken, "vhub_867");
                assert_eq!(session_name.as_deref(), Some("session-20260611-125634"));
            }
            QuoteOrigin::Hub { .. } => panic!("expected client origin"),
        }
    }

    /// Identical text from two sessions is disambiguated by the quoted timestamp.
    #[test]
    fn resolve_by_content_uses_timestamp_to_disambiguate() {
        let mut idx = QuoteRouteIndex::default();
        let text = "完成了";
        idx.by_content.insert(
            format!("full:{text}"),
            vec![
                ContentEntry {
                    scope: SCOPE.into(),
                    origin: QuoteOrigin::Client {
                        vtoken: "vt_old".into(),
                        name: "a".into(),
                        label: None,
                        session_name: Some("s-old".into()),
                    },
                    created_ms: 1_000_000_000_000,
                    deadline: Instant::now() + INDEX_TTL,
                },
                ContentEntry {
                    scope: SCOPE.into(),
                    origin: QuoteOrigin::Client {
                        vtoken: "vt_new".into(),
                        name: "b".into(),
                        label: None,
                        session_name: Some("s-new".into()),
                    },
                    created_ms: 1_000_000_050_000,
                    deadline: Instant::now() + INDEX_TTL,
                },
            ],
        );

        let origin = idx
            .resolve_by_content(SCOPE, text, Some(1_000_000_001_000))
            .expect("resolve");
        match origin {
            QuoteOrigin::Client { vtoken, .. } => assert_eq!(vtoken, "vt_old"),
            _ => panic!("expected client"),
        }
    }

    /// A quote-reply in conversation B must NOT resolve to a message the Hub sent into
    /// conversation A, even when the quoted text is identical.
    #[test]
    fn resolve_is_scoped_per_conversation() {
        let mut idx = QuoteRouteIndex::default();
        let sent = "你好\n\n— ilink-claude";
        idx.register_outbound_content(
            "userA@x",
            sent,
            QuoteOrigin::Client {
                vtoken: "vt_for_A".into(),
                name: "ilink-claude".into(),
                label: None,
                session_name: Some("default".into()),
            },
        );

        // Same quoted text, but the reply comes from a different conversation.
        let user_b = quote_reply("再来", sent, Some(1781153810000));
        assert!(idx.resolve_user_quote("userB@x", &user_b).is_none());
        // The original conversation still resolves.
        let user_a = quote_reply("再来", sent, Some(1781153810000));
        assert!(idx.resolve_user_quote("userA@x", &user_a).is_some());
    }

    #[test]
    fn resolve_hub_origin_from_content() {
        let mut idx = QuoteRouteIndex::default();
        let sent = "iLink Hub 帮助\n...";
        idx.register_outbound_content(
            SCOPE,
            sent,
            QuoteOrigin::Hub {
                cmd: HubCommand::Help,
            },
        );
        let user = quote_reply("再说一遍", sent, None);
        assert!(matches!(
            idx.resolve_user_quote(SCOPE, &user),
            Some(QuoteOrigin::Hub {
                cmd: HubCommand::Help
            })
        ));
    }
}
