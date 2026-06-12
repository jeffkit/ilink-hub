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

pub fn hub_status(online: usize, total: usize) -> String {
    format!("iLink Hub 状态：{online}/{total} 个客户端在线")
}
