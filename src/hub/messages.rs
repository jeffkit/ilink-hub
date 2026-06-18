//! User-facing message strings for hub commands.
//!
//! Centralising these here keeps business logic files free of UI text and makes
//! future i18n or copy changes easier to manage.

// ─── Generic errors ───────────────────────────────────────────────────────────

pub const NO_BACKEND: &str = "❌ 当前未路由到任何后端，请先用 `/use <名称>` 切换到一个后端。";

pub const UNRECOGNIZED_COMMAND: &str = "未识别的指令。发送 /help 查看可用指令。";

// ─── /session list ────────────────────────────────────────────────────────────

pub const SESSION_LIST_NO_SESSIONS: &str =
    "当前后端尚无 session 记录。\n发送 `/session new <名称>` 创建一个 session。";

pub const SESSION_LIST_SWITCH_HINT: &str =
    "\n用 `/session use <名称>` 切换，`/session new <名称>` 新建。";

pub const SESSION_SLOT_NO_UUID: &str = "（尚无 UUID，下次对话时由后端写入）";

// ─── /session new ─────────────────────────────────────────────────────────────

pub fn session_new_ok(name: &str) -> String {
    format!("✅ 已在当前后端创建并切换到 session `{name}`。")
}

pub fn session_new_created_switch_failed(name: &str, e: &dyn std::fmt::Display) -> String {
    format!("✅ 已创建 session `{name}`，但切换失败：{e}")
}

pub fn session_new_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 创建 session 失败：{e}")
}

// ─── /session use ─────────────────────────────────────────────────────────────

pub fn session_use_ok(name: &str) -> String {
    format!("✅ 已切换到 session `{name}`")
}

pub fn session_use_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 切换 session 失败：{e}")
}

pub fn session_use_slot_create_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 创建 session slot 失败：{e}")
}

pub fn session_use_query_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 查询 session 失败：{e}")
}

// ─── /session delete ─────────────────────────────────────────────────────────

pub fn session_delete_active_error(name: &str) -> String {
    format!(
        "❌ 无法删除当前活跃的 session `{name}`。\n请先用 `/session use <其他名称>` 切换后再删除。"
    )
}

pub fn session_delete_ok(name: &str) -> String {
    format!("✅ 已删除 session `{name}`")
}

pub fn session_delete_not_found(name: &str) -> String {
    format!("❌ 未找到 session `{name}`")
}

pub fn session_delete_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 删除 session 失败：{e}")
}

pub fn session_list_failed(e: &dyn std::fmt::Display) -> String {
    format!("❌ 查询 session 失败：{e}")
}

// ─── /status ─────────────────────────────────────────────────────────────────

/// `sessions`: `(client_name, session_name, last_user_msg, waiting_for_reply, user_msg_created_at)`.
/// Only online clients are included — offline ones are omitted from the overview.
///
/// `waiting_for_reply = true` means the user sent a message but the AI has not
/// replied yet — shown as "⏳ 处理中 (elapsed)".
pub fn hub_status(
    online: usize,
    total: usize,
    client_sessions: &[(String, Vec<crate::store::SessionStatusEntry>)],
) -> String {
    let mut lines = vec![format!("iLink Hub 状态：{online}/{total} 个客户端在线")];
    if !client_sessions.is_empty() {
        lines.push(String::new());
        lines.push("**会话列表：**".to_string());
        for (name, sessions) in client_sessions {
            if sessions.is_empty() {
                lines.push(format!("🟢 `{name}`\n  └ （无会话记录）"));
            } else {
                lines.push(format!("🟢 `{name}`"));
                for entry in sessions {
                    let session = &entry.session_name;
                    let snippet = entry
                        .last_user_content
                        .as_deref()
                        .unwrap_or("（无消息记录）");
                    let truncated = if snippet.chars().count() > 30 {
                        let s: String = snippet.chars().take(30).collect();
                        format!("{s}…")
                    } else {
                        snippet.to_string()
                    };
                    let status_tag = if entry.waiting_for_reply {
                        let elapsed = entry
                            .user_msg_created_at
                            .as_deref()
                            .and_then(parse_elapsed_secs)
                            .map(format_elapsed)
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default();
                        format!(" ⏳{elapsed}")
                    } else {
                        String::new()
                    };
                    lines.push(format!("  └ [{session}]{status_tag} {truncated}"));
                }
            }
        }
    }
    lines.join("\n")
}

/// Parse an ISO-8601 / SQLite CURRENT_TIMESTAMP string and return elapsed seconds since then.
fn parse_elapsed_secs(ts: &str) -> Option<u64> {
    use chrono::{DateTime, NaiveDateTime, Utc};
    // SQLite CURRENT_TIMESTAMP format: "YYYY-MM-DD HH:MM:SS"
    let ndt = NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    let then: DateTime<Utc> = DateTime::from_naive_utc_and_offset(ndt, Utc);
    let secs = (Utc::now() - then).num_seconds().max(0) as u64;
    Some(secs)
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionStatusEntry;

    fn entry(
        session_name: &str,
        last_msg: Option<&str>,
        waiting: bool,
        ts: Option<&str>,
    ) -> SessionStatusEntry {
        SessionStatusEntry {
            session_name: session_name.to_string(),
            last_user_content: last_msg.map(str::to_string),
            waiting_for_reply: waiting,
            user_msg_created_at: ts.map(str::to_string),
        }
    }

    #[test]
    fn hub_status_no_clients() {
        let out = hub_status(0, 0, &[]);
        assert_eq!(out, "iLink Hub 状态：0/0 个客户端在线");
    }

    #[test]
    fn hub_status_online_count_shows_correctly() {
        let client_sessions = vec![
            (
                "claude".to_string(),
                vec![entry("feature-a", Some("帮我看看"), false, None)],
            ),
            (
                "cursor".to_string(),
                vec![entry("default", Some("另一个问题"), false, None)],
            ),
        ];
        let out = hub_status(2, 3, &client_sessions);
        assert!(out.contains("2/3 个客户端在线"));
        assert!(out.contains("🟢 `claude`"));
        assert!(out.contains("🟢 `cursor`"));
        assert!(out.contains("[feature-a]"));
        assert!(out.contains("[default]"));
    }

    #[test]
    fn hub_status_client_multiple_sessions() {
        let client_sessions = vec![(
            "claude".to_string(),
            vec![
                entry("feature-a", Some("帮我看一下这段代码"), false, None),
                entry("default", Some("另一个问题"), true, None),
            ],
        )];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("1/1 个客户端在线"));
        assert!(out.contains("🟢 `claude`"));
        assert!(out.contains("[feature-a]"));
        assert!(out.contains("[default]"));
        assert!(out.contains("帮我看一下这段代码"));
        assert!(out.contains("另一个问题"));
        assert!(out.contains("⏳"));
    }

    #[test]
    fn hub_status_client_idle() {
        let client_sessions = vec![(
            "claude".to_string(),
            vec![entry("feature-a", Some("帮我看一下这段代码"), false, None)],
        )];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("1/1 个客户端在线"));
        assert!(out.contains("🟢 `claude`"));
        assert!(out.contains("[feature-a]"));
        assert!(!out.contains("⏳"));
        assert!(out.contains("帮我看一下这段代码"));
    }

    #[test]
    fn hub_status_client_waiting_no_timestamp() {
        let client_sessions = vec![(
            "cursor".to_string(),
            vec![entry("default", Some("请帮我优化这个函数"), true, None)],
        )];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("⏳"), "expected ⏳ in: {out}");
        assert!(out.contains("请帮我优化这个函数"));
        assert!(!out.contains('('), "no elapsed bracket without timestamp");
    }

    #[test]
    fn hub_status_client_waiting_with_timestamp() {
        use chrono::{Duration, Utc};
        let ts = (Utc::now() - Duration::seconds(125))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let client_sessions = vec![(
            "bot".to_string(),
            vec![entry("default", Some("这个bug怎么修"), true, Some(&ts))],
        )];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("⏳"), "expected ⏳ in: {out}");
        assert!(out.contains("2m"), "expected minutes in elapsed: {out}");
    }

    #[test]
    fn hub_status_long_message_truncated() {
        // 31 Chinese characters — over the 30-char limit.
        let long_msg =
            "这是一条超过三十个汉字用于测试截断逻辑是否正确的确不应该完整显示".to_string();
        assert!(
            long_msg.chars().count() > 30,
            "test string must be > 30 chars, got {}",
            long_msg.chars().count()
        );
        let client_sessions = vec![(
            "bot".to_string(),
            vec![entry("default", Some(&long_msg), false, None)],
        )];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("…"), "expected truncation ellipsis in: {out}");
        let snippet_line = out.lines().find(|l| l.contains("└")).unwrap();
        // strip "  └ [default] "
        let snippet = snippet_line
            .trim_start_matches("  └ ")
            .split_once("] ")
            .map(|x| x.1)
            .unwrap_or(snippet_line);
        assert!(
            snippet.chars().count() <= 31,
            "snippet too long ({} chars): {snippet}",
            snippet.chars().count()
        );
    }

    #[test]
    fn hub_status_no_message_record() {
        let client_sessions = vec![("agy".to_string(), vec![entry("default", None, false, None)])];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("（无消息记录）"));
    }

    #[test]
    fn hub_status_client_no_sessions() {
        let client_sessions = vec![("agy".to_string(), vec![])];
        let out = hub_status(1, 1, &client_sessions);
        assert!(out.contains("（无会话记录）"));
    }

    #[test]
    fn format_elapsed_under_minute() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(45), "45s");
        assert_eq!(format_elapsed(59), "59s");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(60), "1m0s");
        assert_eq!(format_elapsed(125), "2m5s");
        assert_eq!(format_elapsed(3661), "61m1s");
    }

    #[test]
    fn parse_elapsed_secs_valid_sqlite_format() {
        use chrono::{Duration, Utc};
        let ts = (Utc::now() - Duration::seconds(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let secs = parse_elapsed_secs(&ts).unwrap();
        // Allow ±2s for test execution lag.
        assert!((28..=32).contains(&secs), "expected ~30s, got {secs}");
    }

    #[test]
    fn parse_elapsed_secs_invalid_returns_none() {
        assert!(parse_elapsed_secs("not-a-date").is_none());
        assert!(parse_elapsed_secs("").is_none());
    }
}
