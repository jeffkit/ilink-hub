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
use std::hash::{BuildHasher, Hash};
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
    /// Logical sequence number to break ties and ensure deterministic eviction.
    seq: u64,
    deadline: Instant,
}

/// In-memory index with TTL eviction (no persistence yet).
#[derive(Debug)]
pub struct QuoteRouteIndex {
    /// Content signature hash → outbound origins (scoped per conversation).
    by_content: HashMap<u64, Vec<ContentEntry>>,
    /// Monotonically increasing counter to assign unique logical sequence numbers.
    next_seq: u64,
    /// Per-instance random seed so content hashes are not predictable externally.
    hasher: std::collections::hash_map::RandomState,
}

impl Default for QuoteRouteIndex {
    fn default() -> Self {
        Self {
            by_content: HashMap::new(),
            next_seq: 0,
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }
}

const INDEX_TTL: Duration = Duration::from_secs(86400 * 7);
/// Cap entries per content signature to bound memory when the same text is sent repeatedly.
const MAX_CONTENT_ENTRIES_PER_KEY: usize = 32;
/// Length (in chars) of the prefix key used to tolerate WeChat truncating long quoted text.
const CONTENT_PREFIX_CHARS: usize = 48;
/// Cap the total number of content signature keys in the index to prevent memory exhaustion.
const MAX_BY_CONTENT_KEYS: usize = 10_000;

impl QuoteRouteIndex {
    fn hash_key<T: Hash>(&self, t: &T) -> u64 {
        self.hasher.hash_one(t)
    }

    /// Index an outbound message by its exact rendered text so a later quote-reply in the
    /// same conversation routes back to its origin.
    ///
    /// * `scope` — conversation key (the WeChat `from_user_id`, or group key).
    /// * `text` — the final body actually sent to iLink (after any origin footer), since that
    ///   is exactly what WeChat reproduces inside `ref_msg`.
    pub fn register_outbound_content(&mut self, scope: &str, text: &str, origin: QuoteOrigin) {
        let now_ms = now_millis();
        let deadline = Instant::now() + INDEX_TTL;

        // Evict expired entries first to free up space.
        self.evict_expired();

        for key_str in content_keys(text) {
            let key = self.hash_key(&key_str);
            if !self.by_content.contains_key(&key) && self.by_content.len() >= MAX_BY_CONTENT_KEYS {
                // Find the oldest key to evict.
                let mut oldest_key: Option<u64> = None;
                let mut oldest_ms = i64::MAX;
                let mut oldest_seq = u64::MAX;
                for (k, bucket) in &self.by_content {
                    if let Some(entry) = bucket.iter().min_by_key(|e| (e.created_ms, e.seq)) {
                        if entry.created_ms < oldest_ms
                            || (entry.created_ms == oldest_ms && entry.seq < oldest_seq)
                        {
                            oldest_ms = entry.created_ms;
                            oldest_seq = entry.seq;
                            oldest_key = Some(*k);
                        }
                    }
                }
                if let Some(k) = oldest_key {
                    self.by_content.remove(&k);
                }
            }
            let bucket = self.by_content.entry(key).or_default();
            bucket.push(ContentEntry {
                scope: scope.to_string(),
                origin: origin.clone(),
                created_ms: now_ms,
                seq: self.next_seq,
                deadline,
            });
            self.next_seq = self.next_seq.wrapping_add(1);
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
        for key_str in content_keys(text) {
            let key = self.hash_key(&key_str);
            let Some(bucket) = self.by_content.get(&key) else {
                continue;
            };
            let best = bucket
                .iter()
                .filter(|e| now <= e.deadline && e.scope == scope)
                .min_by_key(|e| match ref_ms {
                    Some(ms) => (e.created_ms as i128 - ms as i128).abs(),
                    // No quoted timestamp: prefer the most recent send (smallest negative age).
                    None => -(e.created_ms as i128),
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

    /// Extract the quoted text and timestamp from a user message's ref_msg.
    /// Public so DB-backed fallback resolvers can reuse the same extraction logic.
    pub fn collect_quoted(msg: &crate::ilink::types::WeixinMessage) -> Option<(String, Option<i64>)> {
        collect_quoted_content(msg)
    }

    /// Extract `(backend_name, session_name)` from the footer embedded in the quoted message
    /// text. Used as a fallback when the in-memory index is cold (e.g. after a Hub restart).
    pub fn footer_from_user_quote(
        msg: &crate::ilink::types::WeixinMessage,
    ) -> Option<(String, Option<String>)> {
        let (text, _) = collect_quoted_content(msg)?;
        parse_footer_from_quoted_text(&text)
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

/// Fallback when the in-memory quote index is cold (e.g. after a Hub restart): parse the
/// outbound origin footer embedded in the quoted message text and return `(backend_name,
/// session_name)`.
///
/// Handles two historical footer formats:
/// * New (current): `…\n\n---\n{name} [· label] [· session]`
/// * Old (pre-footer-hr): `…\n\n— {name} [· session]`
///
/// Returns `None` when no recognisable footer is found.
pub fn parse_footer_from_quoted_text(text: &str) -> Option<(String, Option<String>)> {
    // Try new format: last line after `---` separator.
    let footer_line = if let Some(pos) = text.rfind("\n---\n") {
        text[pos + 5..].trim()
    } else if let Some(pos) = text.rfind("\n— ") {
        // Old format: line starting with em-dash.
        text[pos + 4..].trim()
    } else if let Some(stripped) = text.trim().strip_prefix("— ") {
        // Edge case: the whole quoted text is just a footer line.
        stripped.trim()
    } else {
        return None;
    };

    if footer_line.is_empty() {
        return None;
    }

    // Split by ` · ` — parts are [name, label?, session?]
    let parts: Vec<&str> = footer_line.split(" · ").collect();
    let name = parts[0].trim();
    if name.is_empty() {
        return None;
    }

    // The last part that looks like a session name (`at-YYYYMMDD-*` or `session-YYYYMMDD-*`).
    let session = parts.iter().rev().find_map(|p| {
        let p = p.trim();
        if p.starts_with("at-") || p.starts_with("session-") {
            Some(p.to_string())
        } else {
            None
        }
    });

    Some((name.to_string(), session))
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
            idx.hash_key(&format!("full:{text}")),
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
                    seq: 0,
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
                    seq: 1,
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

    #[test]
    fn register_outbound_content_respects_limit() {
        let mut idx = QuoteRouteIndex::default();
        // Register 10,000 distinct messages
        for i in 0..10000 {
            idx.register_outbound_content(
                SCOPE,
                &format!("msg_{}", i),
                QuoteOrigin::Client {
                    vtoken: format!("vt_{}", i),
                    name: "n".into(),
                    label: None,
                    session_name: None,
                },
            );
        }
        assert_eq!(idx.by_content.len(), 10000);

        // Register one more. This should evict the oldest key (msg_0).
        idx.register_outbound_content(
            SCOPE,
            "msg_overflow",
            QuoteOrigin::Client {
                vtoken: "vt_overflow".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            },
        );
        // The count should still be 10,000
        assert_eq!(idx.by_content.len(), 10000);

        // msg_overflow should be present and resolve successfully
        let user = quote_reply("reply", "msg_overflow", None);
        let origin = idx
            .resolve_user_quote(SCOPE, &user)
            .expect("resolve overflow");
        match origin {
            QuoteOrigin::Client { vtoken, .. } => assert_eq!(vtoken, "vt_overflow"),
            _ => panic!("expected client"),
        }

        // msg_0 should be evicted and fail to resolve
        let user_evicted = quote_reply("reply", "msg_0", None);
        assert!(idx.resolve_user_quote(SCOPE, &user_evicted).is_none());

        // However, registering an existing key (like msg_1) should still succeed and update the entries
        idx.register_outbound_content(
            SCOPE,
            "msg_1",
            QuoteOrigin::Client {
                vtoken: "vt_updated".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            },
        );
        assert_eq!(idx.by_content.len(), 10000);
        let user_updated = quote_reply("reply", "msg_1", None);
        let origin = idx
            .resolve_user_quote(SCOPE, &user_updated)
            .expect("resolve");
        match origin {
            QuoteOrigin::Client { vtoken, .. } => assert_eq!(vtoken, "vt_updated"),
            _ => panic!("expected client"),
        }
    }

    #[test]
    fn resolve_by_content_overflow_protection_min() {
        let mut idx = QuoteRouteIndex::default();
        let text = "overflow_min";
        idx.register_outbound_content(
            SCOPE,
            text,
            QuoteOrigin::Client {
                vtoken: "vt".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            },
        );
        // Using i64::MIN as ref_ms should not panic
        let user = quote_reply("reply", text, Some(i64::MIN));
        let origin = idx.resolve_user_quote(SCOPE, &user);
        assert!(origin.is_some());
    }

    #[test]
    fn resolve_by_content_overflow_protection_max() {
        let mut idx = QuoteRouteIndex::default();
        let text = "overflow_max";
        idx.register_outbound_content(
            SCOPE,
            text,
            QuoteOrigin::Client {
                vtoken: "vt".into(),
                name: "n".into(),
                label: None,
                session_name: None,
            },
        );
        // Using i64::MAX as ref_ms should not panic
        let user = quote_reply("reply", text, Some(i64::MAX));
        let origin = idx.resolve_user_quote(SCOPE, &user);
        assert!(origin.is_some());
    }

    #[test]
    fn parse_footer_new_format_name_and_session() {
        let text = "你好！有什么我可以帮你的吗？\n\n---\nilink-claude · session-20260611-125634";
        let (name, session) = parse_footer_from_quoted_text(text).unwrap();
        assert_eq!(name, "ilink-claude");
        assert_eq!(session.as_deref(), Some("session-20260611-125634"));
    }

    #[test]
    fn parse_footer_new_format_with_label() {
        let text = "body\n\n---\nilink-claude · office · session-20260611-194813";
        let (name, session) = parse_footer_from_quoted_text(text).unwrap();
        assert_eq!(name, "ilink-claude");
        assert_eq!(session.as_deref(), Some("session-20260611-194813"));
    }

    #[test]
    fn parse_footer_old_format_em_dash() {
        // The historical "— backend · session" format produced by older Hub versions.
        let text = "你好！有什么我可以帮你的吗？\n\n— ilink-claude · session-20260611-125634";
        let (name, session) = parse_footer_from_quoted_text(text).unwrap();
        assert_eq!(name, "ilink-claude");
        assert_eq!(session.as_deref(), Some("session-20260611-125634"));
    }

    #[test]
    fn parse_footer_at_mention_session() {
        let text = "完成了\n\n---\nilink-claude · at-20260615-114019020";
        let (name, session) = parse_footer_from_quoted_text(text).unwrap();
        assert_eq!(name, "ilink-claude");
        assert_eq!(session.as_deref(), Some("at-20260615-114019020"));
    }

    #[test]
    fn parse_footer_name_only_no_session() {
        let text = "hello\n\n---\nilink-claude";
        let (name, session) = parse_footer_from_quoted_text(text).unwrap();
        assert_eq!(name, "ilink-claude");
        assert!(session.is_none());
    }

    #[test]
    fn parse_footer_no_footer_returns_none() {
        assert!(parse_footer_from_quoted_text("plain message without footer").is_none());
    }
}
