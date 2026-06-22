//! Human-readable origin footer and persona header on downstream `sendmessage` bodies.

use crate::ilink::types::WeixinMessage;

/// Whether to append the origin footer to outbound client replies.
///
/// - **Default** (env unset or empty, or any other non-reserved value): append only when
///   **more than one client is online** (multiple active backends).
/// - `ILINKHUB_OUTBOUND_ORIGIN_LABEL=0` / `false` / `no` / `off` — never append.
/// - `ILINKHUB_OUTBOUND_ORIGIN_LABEL=1` / `true` / `yes` / `on` — always append (even with one client).
pub fn should_append_outbound_origin_label(
    online_client_count: usize,
    env_val: Option<&str>,
) -> bool {
    let v = env_val.map(str::trim).filter(|s| !s.is_empty());
    match v.map(|s| s.to_ascii_lowercase()).as_deref() {
        None => online_client_count > 1,
        Some("0" | "false" | "no" | "off") => false,
        Some("1" | "true" | "yes" | "on") => true,
        Some(_) => online_client_count > 1,
    }
}

/// Single-line display: `name` or `name · label` when label differs and is non-empty.
pub fn format_outbound_origin_line(name: &str, label: Option<&str>) -> String {
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) if l != name => format!("{name} · {l}"),
        _ => name.to_string(),
    }
}

/// Full footer line: `workspace [· session]` where session is omitted when it equals "default".
pub fn format_outbound_footer(
    name: &str,
    label: Option<&str>,
    session_name: Option<&str>,
) -> String {
    let workspace = format_outbound_origin_line(name, label);
    match session_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default")
    {
        Some(s) => format!("{workspace} · {s}"),
        None => workspace,
    }
}

/// Build the persona header prepended to every non-empty reply when a client has a persona configured.
///
/// Format: `"{emoji} 【name】\n"` (with emoji) or `"【name】\n"` (without).
/// The name is wrapped in 【】 for a bold-like visual effect in WeChat (which does not support Markdown).
/// Returns `None` when `persona_name` is absent or blank.
pub fn build_persona_header(
    persona_name: Option<&str>,
    persona_emoji: Option<&str>,
) -> Option<String> {
    let name = persona_name?.trim();
    if name.is_empty() {
        return None;
    }
    let display = match persona_emoji.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => format!("{e} **{name}**"),
        None => format!("**{name}**"),
    };
    Some(format!("{display}\n\n"))
}

/// Footer shown when persona is active: only the session name (skipping backend name/label
/// since the persona header already identifies the sender).
///
/// Returns `None` when session is absent, blank, or "default".
pub fn build_session_only_footer(session_name: Option<&str>) -> Option<String> {
    let s = session_name?.trim();
    if s.is_empty() || s == "default" {
        return None;
    }
    Some(s.to_string())
}

/// Prepends a persona header and appends `\n\n— {footer}` to the first text item.
///
/// - When `persona_name` is set: prepends header, footer shows session only.
/// - When `persona_name` is absent: prepends nothing, footer shows full origin (name · label · session).
pub fn apply_persona_and_footer_to_first_text_item(
    msg: &mut WeixinMessage,
    persona_name: Option<&str>,
    persona_emoji: Option<&str>,
    client_name: &str,
    label: Option<&str>,
    session_name: Option<&str>,
) {
    let Some(items) = msg.item_list.as_mut() else {
        return;
    };
    let items_mut = std::sync::Arc::make_mut(items);
    let Some(first) = items_mut.first_mut() else {
        return;
    };
    let Some(ti) = first.text_item.as_mut() else {
        return;
    };
    let Some(t) = ti.text.as_ref() else {
        return;
    };

    let has_persona = persona_name
        .map(str::trim)
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if has_persona {
        // Persona active: prepend header, append session-only footer (if non-default session).
        let header = build_persona_header(persona_name, persona_emoji).unwrap_or_default();
        let body = format!("{header}{t}");
        ti.text = Some(match build_session_only_footer(session_name) {
            Some(session) => format!("{body}\n\n---\n{session}"),
            None => body,
        });
    } else {
        // No persona: keep existing full-origin footer behaviour.
        let line = format_outbound_footer(client_name, label, session_name);
        ti.text = Some(format!("{t}\n\n---\n{line}"));
    }
}

/// Appends `\n\n— {workspace [· session]}` to the first text item, if present.
pub fn append_outbound_origin_footer_to_first_text_item(
    msg: &mut WeixinMessage,
    name: &str,
    label: Option<&str>,
    session_name: Option<&str>,
) {
    let line = format_outbound_footer(name, label, session_name);
    let Some(items) = msg.item_list.as_mut() else {
        return;
    };
    let items_mut = std::sync::Arc::make_mut(items);
    let Some(first) = items_mut.first_mut() else {
        return;
    };
    let Some(ti) = first.text_item.as_mut() else {
        return;
    };
    let Some(t) = ti.text.as_ref() else {
        return;
    };
    ti.text = Some(format!("{t}\n\n---\n{line}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem};

    #[test]
    fn should_append_defaults_multi_only() {
        assert!(!should_append_outbound_origin_label(0, None));
        assert!(!should_append_outbound_origin_label(1, None));
        assert!(should_append_outbound_origin_label(2, None));
        assert!(should_append_outbound_origin_label(3, None));
    }

    #[test]
    fn should_append_force_off() {
        assert!(!should_append_outbound_origin_label(99, Some("0")));
        assert!(!should_append_outbound_origin_label(99, Some("false")));
        assert!(!should_append_outbound_origin_label(99, Some("OFF")));
        assert!(!should_append_outbound_origin_label(1, Some("no")));
    }

    #[test]
    fn should_append_force_on() {
        assert!(should_append_outbound_origin_label(1, Some("1")));
        assert!(should_append_outbound_origin_label(1, Some("true")));
        assert!(should_append_outbound_origin_label(1, Some("ON")));
    }

    #[test]
    fn should_append_unknown_env_falls_back_to_multi() {
        assert!(!should_append_outbound_origin_label(1, Some("maybe")));
        assert!(should_append_outbound_origin_label(2, Some("maybe")));
    }

    #[test]
    fn format_line_name_only() {
        assert_eq!(format_outbound_origin_line("echo", None), "echo");
        assert_eq!(format_outbound_origin_line("echo", Some("")), "echo");
        assert_eq!(format_outbound_origin_line("echo", Some("   ")), "echo");
    }

    #[test]
    fn format_line_with_distinct_label() {
        assert_eq!(
            format_outbound_origin_line("echo", Some("echo test")),
            "echo · echo test"
        );
    }

    #[test]
    fn format_line_same_label_skipped() {
        assert_eq!(format_outbound_origin_line("echo", Some("echo")), "echo");
    }

    // ─── build_persona_header ──────────────────────────────────────────

    #[test]
    fn persona_header_with_emoji_and_name() {
        let h = build_persona_header(Some("Claude"), Some("🤖")).unwrap();
        assert_eq!(h, "🤖 **Claude**\n\n");
    }

    #[test]
    fn persona_header_name_only() {
        let h = build_persona_header(Some("助手"), None).unwrap();
        assert_eq!(h, "**助手**\n\n");
    }

    #[test]
    fn persona_header_empty_emoji_treated_as_none() {
        let h = build_persona_header(Some("Bot"), Some("  ")).unwrap();
        assert_eq!(h, "**Bot**\n\n");
    }

    #[test]
    fn persona_header_none_or_blank_name_returns_none() {
        assert!(build_persona_header(None, Some("🤖")).is_none());
        assert!(build_persona_header(Some(""), None).is_none());
        assert!(build_persona_header(Some("  "), None).is_none());
    }

    // ─── apply_persona_and_footer_to_first_text_item ──────────────────

    fn text_msg(body: &str) -> WeixinMessage {
        WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some(body.into()),
                }),
                ..Default::default()
            }])),
            ..Default::default()
        }
    }

    #[test]
    fn apply_with_persona_prepends_header_and_session_footer() {
        let mut msg = text_msg("hello");
        apply_persona_and_footer_to_first_text_item(
            &mut msg,
            Some("Claude"),
            Some("🤖"),
            "claude-backend",
            None,
            Some("feature-a"),
        );
        assert_eq!(
            msg.text(),
            Some("🤖 **Claude**\n\nhello\n\n---\nfeature-a")
        );
    }

    #[test]
    fn apply_with_persona_default_session_omits_footer() {
        let mut msg = text_msg("hi");
        apply_persona_and_footer_to_first_text_item(
            &mut msg,
            Some("Claude"),
            Some("🤖"),
            "claude-backend",
            None,
            Some("default"),
        );
        assert_eq!(msg.text(), Some("🤖 **Claude**\n\nhi"));
    }

    #[test]
    fn apply_without_persona_uses_full_origin_footer() {
        let mut msg = text_msg("body");
        apply_persona_and_footer_to_first_text_item(
            &mut msg,
            None,
            None,
            "my-backend",
            Some("my-label"),
            Some("feat"),
        );
        assert_eq!(
            msg.text(),
            Some("body\n\n---\nmy-backend · my-label · feat")
        );
    }

    #[test]
    fn append_footer_mutates_first_text() {
        let mut msg = WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("body".into()),
                }),
                ..Default::default()
            }])),
            ..Default::default()
        };
        append_outbound_origin_footer_to_first_text_item(&mut msg, "w", Some("lbl"), None);
        assert_eq!(msg.text(), Some("body\n\n---\nw · lbl"));
    }

    #[test]
    fn format_footer_with_session() {
        assert_eq!(
            format_outbound_footer("echo", None, Some("feature-a")),
            "echo · feature-a"
        );
        assert_eq!(
            format_outbound_footer("echo", Some("lbl"), Some("feature-a")),
            "echo · lbl · feature-a"
        );
        // "default" session is suppressed
        assert_eq!(
            format_outbound_footer("echo", None, Some("default")),
            "echo"
        );
        assert_eq!(format_outbound_footer("echo", None, None), "echo");
    }
}
