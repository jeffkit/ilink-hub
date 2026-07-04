//! Quote-aware routing: map a WeChat quote-reply back to the backend (or Hub) that
//! produced the quoted message.
//!
//! **Why DB-backed?** The real iLink `getupdates` stream does **not** echo bot
//! messages back, and the `ref_msg.message_item` carried by a user's quote-reply contains
//! **no `msg_id` / `message_id`** — only the quoted text plus second-granularity
//! timestamps. Routing therefore relies on three DB fallback layers:
//!
//! * **L1** Timestamp lookup — `ref_msg.create_time_ms` ± 10 s window against `messages`.
//! * **L2** Content-prefix lookup — 48-char prefix `LIKE` match against `messages`.
//! * **L3** Footer text parsing — parse the embedded `--- name · session` footer.

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

/// DB-backed quote resolver (L3 footer parsing layer). Called after timestamp and
/// content-prefix lookups both miss. Parse the outbound origin footer embedded in the
/// quoted message text and return `(backend_name, session_name)`.
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

/// Extract the quoted text and timestamp from a user message's ref_msg.
/// Public so DB-backed fallback resolvers can reuse the same extraction logic.
pub fn collect_quoted(msg: &crate::ilink::types::WeixinMessage) -> Option<(String, Option<i64>)> {
    collect_quoted_content(msg)
}

/// Extract `(backend_name, session_name)` from the footer embedded in the quoted message
/// text. DB-backed quote resolver (L3 footer parsing layer). Called after timestamp and
/// content-prefix lookups both miss.
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
