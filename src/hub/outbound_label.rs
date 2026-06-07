//! Human-readable origin footer on downstream `sendmessage` bodies (`— workspace · label`).

use crate::ilink::types::WeixinMessage;

/// Whether to append the origin footer to outbound client replies.
///
/// - **Default** (env unset or empty, or any other non-reserved value): append only when
///   `registered_client_count > 1` (multiple registered backends).
/// - `ILINKHUB_OUTBOUND_ORIGIN_LABEL=0` / `false` / `no` / `off` — never append.
/// - `ILINKHUB_OUTBOUND_ORIGIN_LABEL=1` / `true` / `yes` / `on` — always append (even with one client).
pub fn should_append_outbound_origin_label(
    registered_client_count: usize,
    env_val: Option<&str>,
) -> bool {
    let v = env_val.map(str::trim).filter(|s| !s.is_empty());
    match v.map(|s| s.to_ascii_lowercase()).as_deref() {
        None => registered_client_count > 1,
        Some("0" | "false" | "no" | "off") => false,
        Some("1" | "true" | "yes" | "on") => true,
        Some(_) => registered_client_count > 1,
    }
}

/// Single-line display: `name` or `name · label` when label differs and is non-empty.
pub fn format_outbound_origin_line(name: &str, label: Option<&str>) -> String {
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) if l != name => format!("{name} · {l}"),
        _ => name.to_string(),
    }
}

/// Appends `\n\n— {line}` to the first text item, if present.
pub fn append_outbound_origin_footer_to_first_text_item(
    msg: &mut WeixinMessage,
    name: &str,
    label: Option<&str>,
) {
    let line = format_outbound_origin_line(name, label);
    let Some(items) = msg.item_list.as_mut() else {
        return;
    };
    let Some(first) = items.first_mut() else {
        return;
    };
    let Some(ti) = first.text_item.as_mut() else {
        return;
    };
    let Some(t) = ti.text.as_ref() else {
        return;
    };
    ti.text = Some(format!("{t}\n\n— {line}"));
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
        assert_eq!(
            format_outbound_origin_line("echo", Some("echo")),
            "echo"
        );
    }

    #[test]
    fn append_footer_mutates_first_text() {
        let mut msg = WeixinMessage {
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("body".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
            }]),
            ..Default::default()
        };
        append_outbound_origin_footer_to_first_text_item(&mut msg, "w", Some("lbl"));
        assert_eq!(msg.text(), Some("body\n\n— w · lbl"));
    }
}
