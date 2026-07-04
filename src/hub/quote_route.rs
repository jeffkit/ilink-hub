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

use std::collections::{BTreeSet, HashMap};
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
    /// Global oldest-first ordering across all `(created_ms, seq, key)` tuples, used to evict
    /// the oldest content key in O(log N) instead of scanning the entire `by_content` map.
    /// The key tuple is included so two entries with the same `(created_ms, seq)` still
    /// disambiguate; equality is structural so the BTreeSet stays consistent.
    by_age: BTreeSet<(i64, u64, u64)>,
}

impl Default for QuoteRouteIndex {
    fn default() -> Self {
        Self {
            by_content: HashMap::new(),
            next_seq: 0,
            hasher: std::collections::hash_map::RandomState::new(),
            by_age: BTreeSet::new(),
        }
    }
}

/// One item for [`QuoteRouteIndex::warm_from_history`]: a previously-sent
/// outbound message we want to re-index on startup so a quote-reply arriving
/// right after Hub boot still resolves without waiting for a new outbound
/// message to fill the gap.
///
/// The fields mirror what [`QuoteRouteIndex::register_outbound_content`]
/// takes, plus the pre-built [`QuoteOrigin`] — the index itself stays unaware
/// of where the origin came from (DB row vs. live dispatch path).
#[derive(Debug, Clone)]
pub struct WarmItem {
    pub scope: String,
    pub text: String,
    pub origin: QuoteOrigin,
}

const INDEX_TTL: Duration = Duration::from_secs(86400 * 7);
/// Cap entries per content signature to bound memory when the same text is sent repeatedly.
const MAX_CONTENT_ENTRIES_PER_KEY: usize = 32;
/// Length (in chars) of the prefix key used to tolerate WeChat truncating long quoted text.
const CONTENT_PREFIX_CHARS: usize = 48;
/// Cap the total number of content signature keys in the index to prevent memory exhaustion.
const MAX_BY_CONTENT_KEYS: usize = 10_000;
/// Default number of recent outbound messages to replay into the in-memory
/// `QuoteRouteIndex` on Hub startup. 500 covers the most recent 8 hours of
/// typical iLink traffic (≈ one message/minute) while staying well below the
/// 10 000-key memory cap. Tunable via `ILINK_QUOTE_INDEX_WARMUP_LIMIT`.
pub const DEFAULT_QUOTE_INDEX_WARMUP_LIMIT: i64 = 500;

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
                // O(log N): the BTreeSet is kept in (created_ms, seq, key) order, so the
                // oldest entry is always at the front. Pop one and remove that key from
                // the content map. If by_age and by_content have drifted (e.g. an
                // entry was removed from by_content but its age tuple remains), the
                // entry is silently skipped — see evict_expired for the reconciliation.
                if let Some(&(oldest_ms, oldest_seq, oldest_key)) = self.by_age.iter().next() {
                    self.by_content.remove(&oldest_key);
                    self.by_age.remove(&(oldest_ms, oldest_seq, oldest_key));
                }
            }
            let bucket = self.by_content.entry(key).or_default();
            let seq = self.next_seq;
            bucket.push(ContentEntry {
                scope: scope.to_string(),
                origin: origin.clone(),
                created_ms: now_ms,
                seq,
                deadline,
            });
            self.by_age.insert((now_ms, seq, key));
            self.next_seq = self.next_seq.wrapping_add(1);
            if bucket.len() > MAX_CONTENT_ENTRIES_PER_KEY {
                let overflow = bucket.len() - MAX_CONTENT_ENTRIES_PER_KEY;
                for entry in bucket.drain(0..overflow) {
                    self.by_age.remove(&(entry.created_ms, entry.seq, key));
                }
            }
        }
    }

    /// Replay previously-sent outbound messages into the index. Used on Hub
    /// startup to close the cold-start gap where a quote-reply arrives before
    /// the next outbound message would re-populate the in-memory cache.
    ///
    /// The replay is equivalent to calling [`register_outbound_content`] for
    /// each item, but the `created_ms` is the **wall clock at replay time**
    /// (not the original `created_at` from the DB). Rationale: the TTL is a
    /// sliding window from "now", and the original `created_at` would let
    /// historical rows outlive the 7-day TTL in edge cases; pinning `now_ms`
    /// keeps the eviction policy uniform between live and replayed entries.
    ///
    /// `items` are processed in the order given. Callers that want a
    /// "newest-first" replay should sort the slice by `created_at` descending
    /// before passing it in — the index itself stays order-agnostic.
    ///
    /// Returns the number of items actually indexed. Items whose `text` is
    /// empty (after the trim inside `content_keys`) are silently skipped, so a
    /// caller can hand in a batch with junk rows without first filtering.
    pub fn warm_from_history(&mut self, items: &[WarmItem]) -> usize {
        let mut indexed = 0;
        for item in items {
            if item.text.trim().is_empty() {
                continue;
            }
            self.register_outbound_content(&item.scope, &item.text, item.origin.clone());
            indexed += 1;
        }
        indexed
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
    pub fn collect_quoted(
        msg: &crate::ilink::types::WeixinMessage,
    ) -> Option<(String, Option<i64>)> {
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

    /// Extract the `create_time_ms` timestamp from the `ref_msg` in a user's quote-reply,
    /// without requiring `text_item` to be present. iLink often omits the text content
    /// from `ref_msg.message_item` but always provides the timestamp, making this the
    /// most reliable signal for quote-reply routing.
    pub fn collect_quoted_timestamp(msg: &crate::ilink::types::WeixinMessage) -> Option<i64> {
        let items = msg.item_list.as_ref()?;
        for item in items.iter() {
            let Some(extra) = item.extra.as_object() else {
                continue;
            };
            let Some(mi) = extra.get("ref_msg").and_then(|r| r.get("message_item")) else {
                continue;
            };
            // create_time_ms may be present even when text_item is absent.
            if let Some(ms) = mi.get("create_time_ms").and_then(|v| v.as_i64()) {
                return Some(ms);
            }
        }
        None
    }

    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        // Walk by_content first (source of truth), removing dead entries and
        // collecting the age tuples we need to drop from by_age. We can't iterate
        // by_age alone because the `(created_ms, seq)` tuple no longer maps back
        // to a single key once that key has been removed.
        let mut dead_age_tuples: Vec<(i64, u64, u64)> = Vec::new();
        for (key, bucket) in self.by_content.iter_mut() {
            for entry in bucket.iter() {
                if entry.deadline <= now {
                    dead_age_tuples.push((entry.created_ms, entry.seq, *key));
                }
            }
            bucket.retain(|e| now <= e.deadline);
        }
        self.by_content.retain(|_, bucket| !bucket.is_empty());
        for t in dead_age_tuples {
            self.by_age.remove(&t);
        }
        // Belt-and-suspenders: if a key vanished (e.g. oldest-eviction path)
        // without removing its by_age entry, the front of by_age may now point
        // to a non-existent key. Drain any orphan tuples whose key is missing
        // from by_content. Capped at one sweep per call to bound work in the
        // face of large drift; remaining orphans are cleaned on the next call.
        let orphans: Vec<(i64, u64, u64)> = self
            .by_age
            .iter()
            .take_while(|t| !self.by_content.contains_key(&t.2))
            .copied()
            .collect();
        for t in orphans {
            self.by_age.remove(&t);
        }
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

/// Convert a freshly-loaded [`RecentOutboundRow`] into a [`WarmItem`] ready for
/// [`QuoteRouteIndex::warm_from_history`].
///
/// The conversion needs the *name* of the originating client (the `QuoteOrigin::Client`
/// `name` field) which the `messages` table does not store directly — it is only
/// embedded in the rendered text footer. We re-parse the footer with
/// [`parse_footer_from_quoted_text`]; rows whose footer is missing or unrecognised
/// fall back to a [`QuoteOrigin::Client`] with a synthetic `<warmup>` name so at
/// least the vtoken (the load-bearing routing field) is preserved.
///
/// Rows without a `vtoken` (legacy / migration / system messages) return `None`
/// and are skipped by the caller. Indexing them as `HubCommand::Help` would
/// cause a quote-reply on such messages to incorrectly trigger `/help`, which is
/// worse than the correct DB-fallback path that fires when no in-memory entry
/// is found.
pub fn warm_item_from_recent_row(row: &crate::store::RecentOutboundRow) -> Option<WarmItem> {
    let origin = match (
        row.vtoken.as_deref(),
        parse_footer_from_quoted_text(&row.text),
    ) {
        (Some(vt), Some((name, session_name))) => QuoteOrigin::Client {
            vtoken: vt.to_string(),
            name,
            label: None,
            session_name,
        },
        (Some(vt), None) => {
            // No recognisable footer. Fall back to a Client origin with a generic
            // name so at least the vtoken (the load-bearing routing field) is set;
            // operators can still tell from logs that the warmup is missing names
            // for footer-less historical rows.
            QuoteOrigin::Client {
                vtoken: vt.to_string(),
                name: "<warmup>".to_string(),
                label: None,
                session_name: Some(row.session_name.clone()),
            }
        }
        (None, _) => {
            // No vtoken: skip warmup indexing. A quote-reply will fall back to the
            // DB resolver rather than incorrectly routing to HubCommand::Help.
            return None;
        }
    };
    Some(WarmItem {
        scope: row.from_user.clone(),
        text: row.text.clone(),
        origin,
    })
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
                ..Default::default()
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
                ..Default::default()
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

    // ─── Bug repro: @mention session quote-routing ──────────────────────────
    //
    // Scenario reproduced from production incident (2026-06-24):
    //
    //  1. User sends `@ilink-claude <msg>` → Hub creates at-session "at-20260624-092041904"
    //     and dispatches to vtoken "vt-claude".
    //  2. Bridge replies via sendmessage. The reply is indexed in quote_index with
    //     session_name = Some("at-20260624-092041904") and vtoken = "vt-claude".
    //  3. User quote-replies to that message. The inbound routing must resolve back
    //     to ("vt-claude", "at-20260624-092041904"), NOT fall through to the current
    //     /use route (e.g. "vt-home" pointing at the old "session-20260623-181249").
    //
    // This test pins that the quote_index correctly stores and retrieves the
    // at-session name from the outbound reply footer.

    #[test]
    fn at_mention_reply_quote_routes_back_to_at_session() {
        let mut idx = QuoteRouteIndex::default();
        let at_session = "at-20260624-092041904";
        // Simulate the outbound reply footer that the Hub appends when sending
        // the @mention response back to the user.
        let reply_text = format!(
            "只有 codebuddy 那个文件里提到 GLM...\n\n---\nilink-claude · KONGJIE-MC3 · {at_session}"
        );
        let vtoken_claude = "vt-2f43aec8";

        // Register as if sendmessage handler indexed it.
        idx.register_outbound_content(
            "peer:user@wx",
            &reply_text,
            QuoteOrigin::Client {
                vtoken: vtoken_claude.to_string(),
                name: "ilink-claude".to_string(),
                label: Some("KONGJIE-MC3".to_string()),
                session_name: Some(at_session.to_string()),
            },
        );

        // User quote-replies to that message.
        let inbound = quote_reply("followup question", &reply_text, Some(1750000000000));
        let origin = idx
            .resolve_user_quote("peer:user@wx", &inbound)
            .expect("quote_index must resolve the at-mention reply");

        match origin {
            QuoteOrigin::Client {
                vtoken,
                session_name,
                ..
            } => {
                assert_eq!(
                    vtoken, vtoken_claude,
                    "must route back to ilink-claude, not the home/default backend"
                );
                assert_eq!(
                    session_name.as_deref(),
                    Some(at_session),
                    "must resume the at-session, not fall back to the old active session"
                );
            }
            QuoteOrigin::Hub { .. } => panic!("expected Client origin, got Hub"),
        }
    }

    /// When there are TWO registered sessions for the same vtoken (the old
    /// active session-1249 AND the new at-1904), a quote-reply that quotes the
    /// at-1904 message must NOT be confused with the session-1249 entry.
    #[test]
    fn at_mention_reply_not_confused_with_older_active_session() {
        let mut idx = QuoteRouteIndex::default();
        let scope = "peer:user@wx";
        let vtoken = "vt-2f43aec8";

        // First, there's an existing session-1249 entry from a previous conversation.
        let old_reply = "目前这次对话里完成的事情...\n\n---\nilink-claude · KONGJIE-MC3 · session-20260623-181249";
        idx.register_outbound_content(
            scope,
            old_reply,
            QuoteOrigin::Client {
                vtoken: vtoken.to_string(),
                name: "ilink-claude".to_string(),
                label: Some("KONGJIE-MC3".to_string()),
                session_name: Some("session-20260623-181249".to_string()),
            },
        );

        // Then the @mention reply is indexed (newer, different text).
        let at_reply =
            "只有 codebuddy 那个文件里提到 GLM\n\n---\nilink-claude · KONGJIE-MC3 · at-20260624-092041904";
        idx.register_outbound_content(
            scope,
            at_reply,
            QuoteOrigin::Client {
                vtoken: vtoken.to_string(),
                name: "ilink-claude".to_string(),
                label: Some("KONGJIE-MC3".to_string()),
                session_name: Some("at-20260624-092041904".to_string()),
            },
        );

        // Quote-replying the at-mention reply → must get at-session.
        let q_at = quote_reply("follow up on glm", at_reply, Some(1750000100000));
        let origin_at = idx
            .resolve_user_quote(scope, &q_at)
            .expect("must resolve at-session reply");
        match origin_at {
            QuoteOrigin::Client { session_name, .. } => {
                assert_eq!(session_name.as_deref(), Some("at-20260624-092041904"));
            }
            _ => panic!("expected Client"),
        }

        // Quote-replying the old session-1249 reply → must get session-1249.
        let q_old = quote_reply("continue old session", old_reply, Some(1749900000000));
        let origin_old = idx
            .resolve_user_quote(scope, &q_old)
            .expect("must resolve old session reply");
        match origin_old {
            QuoteOrigin::Client { session_name, .. } => {
                assert_eq!(session_name.as_deref(), Some("session-20260623-181249"));
            }
            _ => panic!("expected Client"),
        }
    }

    // ─── M3 — quote_index startup warmup ───────────────────────────────────

    /// Equivalence with N manual `register_outbound_content` calls. Each warm
    /// item must produce an entry in the index that resolves to the same
    /// `QuoteOrigin` as if we had fed it through the live dispatch path.
    #[test]
    fn warm_from_history_equivalent_to_register_loop() {
        let items = vec![
            WarmItem {
                scope: SCOPE.into(),
                text: "alpha\n\n---\nilink-claude · session-20260611-1".into(),
                origin: QuoteOrigin::Client {
                    vtoken: "vt_a".into(),
                    name: "ilink-claude".into(),
                    label: None,
                    session_name: Some("session-20260611-1".into()),
                },
            },
            WarmItem {
                scope: SCOPE.into(),
                text: "beta\n\n---\nilink-claude · session-20260611-2".into(),
                origin: QuoteOrigin::Client {
                    vtoken: "vt_b".into(),
                    name: "ilink-claude".into(),
                    label: None,
                    session_name: Some("session-20260611-2".into()),
                },
            },
            WarmItem {
                scope: SCOPE.into(),
                text: "list\n\n---\nhub".into(),
                origin: QuoteOrigin::Hub {
                    cmd: HubCommand::List,
                },
            },
        ];

        let mut warm_idx = QuoteRouteIndex::default();
        let n = warm_idx.warm_from_history(&items);
        assert_eq!(n, 3);

        let mut live_idx = QuoteRouteIndex::default();
        for item in &items {
            live_idx.register_outbound_content(&item.scope, &item.text, item.origin.clone());
        }

        // Both indices must resolve each of the three texts to a non-None origin,
        // and the resolved origins must agree with the input WarmItems.
        for item in &items {
            let warm_origin = warm_idx
                .resolve_by_content(SCOPE, &item.text, None)
                .expect("warm resolve");
            let live_origin = live_idx
                .resolve_by_content(SCOPE, &item.text, None)
                .expect("live resolve");
            assert_eq!(
                format!("{:?}", warm_origin),
                format!("{:?}", live_origin),
                "warm and live paths must agree for `{}`",
                item.text
            );
        }
    }

    /// Empty slice is a no-op (returns 0, no panic, no entries created).
    #[test]
    fn warm_from_history_empty_slice_is_noop() {
        let mut idx = QuoteRouteIndex::default();
        assert_eq!(idx.warm_from_history(&[]), 0);
        assert_eq!(idx.by_content.len(), 0);
    }

    /// An item with empty text is silently dropped — `content_keys` would
    /// return an empty Vec, and a literal empty `text` is never what we want
    /// to index.
    #[test]
    fn warm_from_history_skips_empty_text() {
        let mut idx = QuoteRouteIndex::default();
        let items = vec![
            WarmItem {
                scope: SCOPE.into(),
                text: "".into(),
                origin: QuoteOrigin::Client {
                    vtoken: "vt".into(),
                    name: "n".into(),
                    label: None,
                    session_name: None,
                },
            },
            WarmItem {
                scope: SCOPE.into(),
                text: "   ".into(), // whitespace-only — trim().is_empty() is true
                origin: QuoteOrigin::Client {
                    vtoken: "vt2".into(),
                    name: "n".into(),
                    label: None,
                    session_name: None,
                },
            },
        ];
        assert_eq!(idx.warm_from_history(&items), 0);
        assert_eq!(idx.by_content.len(), 0);
    }

    /// Warmup must respect the conversation scope just like the live path:
    /// a quote-reply from a different scope must not resolve to a warmup row
    /// belonging to a different conversation.
    #[test]
    fn warm_from_history_preserves_per_scope_isolation() {
        let mut idx = QuoteRouteIndex::default();
        let items = vec![WarmItem {
            scope: "userA@x".into(),
            text: "ping\n\n---\nilink-claude".into(),
            origin: QuoteOrigin::Client {
                vtoken: "vt_a".into(),
                name: "ilink-claude".into(),
                label: None,
                session_name: None,
            },
        }];
        idx.warm_from_history(&items);

        // Build a real quote-reply whose ref_msg.text matches the warmup text;
        // `resolve_user_quote` is the public entry point and exercises the
        // same scope filter as the live dispatch path.
        let reply_a = quote_reply("userA@x", "ping\n\n---\nilink-claude", None);
        let reply_b = quote_reply("userB@x", "ping\n\n---\nilink-claude", None);
        assert!(idx.resolve_user_quote("userA@x", &reply_a).is_some());
        assert!(idx.resolve_user_quote("userB@x", &reply_b).is_none());
    }

    /// `warm_item_from_recent_row` happy path: footer present, vtoken present
    /// → QuoteOrigin::Client with the parsed name and session.
    #[test]
    fn warm_item_from_recent_row_uses_footer_name_and_session() {
        let row = crate::store::RecentOutboundRow {
            from_user: "user@x".into(),
            text: "hello\n\n---\nilink-claude · session-20260611-1".into(),
            vtoken: Some("vt".into()),
            session_name: "session-20260611-1".into(),
            created_at: "2026-06-11 12:00:00".into(),
        };
        let item = warm_item_from_recent_row(&row).expect("expected Some(WarmItem)");
        assert_eq!(item.scope, "user@x");
        assert_eq!(item.text, row.text);
        match item.origin {
            QuoteOrigin::Client {
                vtoken,
                name,
                session_name,
                ..
            } => {
                assert_eq!(vtoken, "vt");
                assert_eq!(name, "ilink-claude");
                assert_eq!(session_name.as_deref(), Some("session-20260611-1"));
            }
            _ => panic!("expected Client origin"),
        }
    }

    /// Footer present but vtoken missing → None (skip warmup indexing to avoid
    /// incorrectly routing a quote-reply to HubCommand::Help).
    #[test]
    fn warm_item_from_recent_row_missing_vtoken_returns_none() {
        let row = crate::store::RecentOutboundRow {
            from_user: "user@x".into(),
            text: "list\n\n---\nhub".into(),
            vtoken: None,
            session_name: "default".into(),
            created_at: "2026-06-11 12:00:00".into(),
        };
        assert!(warm_item_from_recent_row(&row).is_none());
    }

    /// Footer missing but vtoken present → Client origin with a `<warmup>`
    /// placeholder name. The vtoken (the load-bearing routing field) is
    /// still set, so a quote-reply will at minimum hit the right client
    /// even though we can't disambiguate the session cleanly.
    #[test]
    fn warm_item_from_recent_row_missing_footer_uses_placeholder_name() {
        let row = crate::store::RecentOutboundRow {
            from_user: "user@x".into(),
            text: "no footer here".into(),
            vtoken: Some("vt".into()),
            session_name: "session-20260611-1".into(),
            created_at: "2026-06-11 12:00:00".into(),
        };
        let item = warm_item_from_recent_row(&row).expect("expected Some(WarmItem)");
        match item.origin {
            QuoteOrigin::Client {
                vtoken,
                name,
                session_name,
                ..
            } => {
                assert_eq!(vtoken, "vt");
                assert_eq!(name, "<warmup>");
                assert_eq!(session_name.as_deref(), Some("session-20260611-1"));
            }
            _ => panic!("expected Client origin with placeholder name"),
        }
    }
}
