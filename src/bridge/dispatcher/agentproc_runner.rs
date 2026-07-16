//! Bridge between ilink-hub's dispatcher and `agentproc::run`.
//!
//! This module replaces the old `executor::run_cli` call path: instead of
//! spawning `ilink-hub-bridge profile <type>` as a subprocess and speaking
//! AgentProc over its stdin/stdout, the dispatcher now drives `agentproc::run`
//! directly — either in-process (when the profile maps to a registered
//! executor like `claude-code`) or via spawn (for custom `command:` profiles).
//!
//! ilink-hub-specific concerns stay here:
//! - `permission_default` (allow / deny / ask) — an ilink-hub policy layered
//!   on top of agentproc's `on_permission` callback.
//! - ApprovalBroker Ask flow — pause the turn, prompt WeChat, await reply.
//! - partial forwarding via `watch::Sender<Option<String>>` (streaming to WeChat).
//! - `CliRunSummary`-compatible metrics for the handle.rs logging/dedup path.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use agentproc::{
    run, Attachment as ApAttachment, PermissionDecision, PermissionFuture, Profile as ApProfile,
    RunOptions,
};

use crate::bridge::config::BridgeProfile;
use crate::bridge::ApprovalBroker;
use crate::ilink::types::WeixinMessage;

/// Metrics collected for one agentproc run, mirroring the old CliRunSummary
/// shape so handle.rs's logging / A1-dedup logic is unchanged.
#[derive(Debug, Clone)]
pub(super) struct RunSummary {
    pub duration_ms: u64,
    #[allow(dead_code)]
    pub exit_code: Option<i32>,
    pub partial_count: u32,
    pub body_bytes: usize,
    pub cli_session_present: bool,
    pub error_event: bool,
    pub usage: Option<serde_json::Value>,
}

/// Convert an ilink-hub BridgeProfile to an agentproc Profile.
///
/// `type:` shorthand maps to the matching in-process executor when one is
/// registered (claude-code → executor: claude-code). Custom `command:` /
/// `script:` profiles fall through to the spawn path. ilink-hub-only fields
/// (permission_default, permission_ask_timeout_secs) stay on BridgeProfile —
/// they drive the on_permission closure, not the agentproc Profile.
pub(super) fn to_agentproc_profile(p: &BridgeProfile) -> ApProfile {
    let executor = p.profile_type.as_deref().and_then(executor_name_for_type);
    ApProfile {
        executor,
        command: p.command.clone(),
        args: p.args.clone(),
        cwd: p.cwd.clone(),
        env: p.env.clone(),
        env_allowlist: p.env_allowlist.clone(),
        timeout_secs: p.timeout_secs,
        kill_grace_secs: p.kill_grace_secs,
        max_reply_chars: p.max_reply_chars,
        truncation_suffix: p.truncation_suffix.clone(),
        include_stderr_in_reply: p.include_stderr_in_reply,
        send_error_reply: true,
        streaming: p.streaming,
        permission: p.permission,
    }
}

/// Map an ilink-hub `type:` shorthand to the agentproc executor name, when
/// the executor exists in agentproc-rs. Returns None for types that have no
/// in-process executor (recursive has a bespoke loop and stays on spawn).
fn executor_name_for_type(type_name: &str) -> Option<String> {
    match type_name {
        // agentproc registers the CodeBuddy executor as `codebuddy` (not
        // `codebuddy-code`); the other built-ins share their ilink-hub type
        // name with the agentproc executor name.
        "claude-code" | "codex" | "cursor" | "agy" => Some(type_name.to_string()),
        "codebuddy-code" => Some("codebuddy".to_string()),
        // recursive / unknown: no in-process executor; fall back to spawn.
        _ => None,
    }
}

/// Convert ilink-hub Attachments (from build_attachments) to agentproc
/// Attachments. Field-for-field identical shape.
pub(super) fn to_agentproc_attachments(
    ilink_atts: &[crate::bridge::protocol::Attachment],
) -> Vec<ApAttachment> {
    ilink_atts
        .iter()
        .map(|a| ApAttachment {
            kind: a.kind.clone(),
            url: a.url.clone(),
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
        })
        .collect()
}

/// One turn driven through agentproc::run. Returns (body, cli_session, summary)
/// to match the old run_cli contract so handle.rs changes minimally.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_via_agentproc(
    profile: &BridgeProfile,
    profile_name: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
    from_user: &str,
    attachments: &[ApAttachment],
    partial_tx: watch::Sender<Option<String>>,
    approval_broker: Arc<ApprovalBroker>,
    session_key: String,
) -> Result<(String, Option<String>, RunSummary)> {
    let ap_profile = to_agentproc_profile(profile);

    // partial_count is tracked here via the on_partial callback — it is an
    // ilink-hub IM-policy counter (A1 dedup), not a protocol field.
    let partial_count = Arc::new(AtomicU32::new(0));
    let partial_tx_for_cb = partial_tx.clone();
    let partial_count_for_cb = Arc::clone(&partial_count);
    let session_id_for_cb: Arc<tokio::sync::Mutex<String>> =
        Arc::new(tokio::sync::Mutex::new(session_id.to_string()));

    let on_partial = Arc::new(move |text: String, sid: Option<String>| {
        partial_count_for_cb.fetch_add(1, Ordering::Relaxed);
        if let Some(s) = sid {
            if !s.is_empty() {
                *session_id_for_cb.blocking_lock() = s;
            }
        }
        let _ = partial_tx_for_cb.send(Some(text));
    }) as Arc<dyn Fn(String, Option<String>) + Send + Sync>;

    // on_permission implements ilink-hub's permission_default policy on top
    // of agentproc's callback. Allow/Deny are synchronous; Ask drives the
    // ApprovalBroker (register inbox, prompt WeChat, await reply).
    let permission_default = profile.permission_default;
    let ask_timeout = Duration::from_secs(profile.permission_ask_timeout_secs.max(1));
    let broker = Arc::clone(&approval_broker);
    let partial_tx_for_perm = partial_tx.clone();
    let session_key_for_perm = session_key.clone();
    let profile_name_for_perm = profile_name.to_string();

    let on_permission = Arc::new(
        move |req: agentproc::PermissionRequest| -> PermissionFuture {
            let broker = Arc::clone(&broker);
            let partial_tx = partial_tx_for_perm.clone();
            let session_key = session_key_for_perm.clone();
            let profile_name = profile_name_for_perm.clone();
            let timeout = ask_timeout;
            let default = permission_default;
            Box::pin(async move {
                match default {
                    crate::bridge::protocol::PermissionDefaultPolicy::Allow => {
                        info!(
                            profile = %profile_name,
                            request_id = %req.request_id,
                            tool = %req.tool_name,
                            "permission auto-allowed (permission_default)"
                        );
                        PermissionDecision::allow()
                    }
                    crate::bridge::protocol::PermissionDefaultPolicy::Deny
                    | crate::bridge::protocol::PermissionDefaultPolicy::DenyLogged => {
                        warn!(
                            profile = %profile_name,
                            request_id = %req.request_id,
                            tool = %req.tool_name,
                            input = ?req.input,
                            "permission denied (permission_default)"
                        );
                        PermissionDecision::deny("denied by bridge permission_default policy")
                    }
                    crate::bridge::protocol::PermissionDefaultPolicy::Ask => {
                        let question = format_approval_question(&req);
                        // Register BEFORE prompting so the reply can't beat us.
                        let (mut inbox, guard) = broker.register(session_key);
                        let _ = partial_tx.send(Some(question));
                        info!(
                            profile = %profile_name,
                            request_id = %req.request_id,
                            tool = %req.tool_name,
                            timeout_secs = timeout.as_secs(),
                            "permission ask: prompting user"
                        );
                        let decision = await_user_approval(
                            &mut inbox,
                            &req.request_id,
                            &req.tool_name,
                            timeout,
                            &partial_tx,
                        )
                        .await;
                        drop(guard);
                        decision
                    }
                }
            })
        },
    )
        as Arc<dyn Fn(agentproc::PermissionRequest) -> PermissionFuture + Send + Sync>;

    let opts = RunOptions {
        message: message.to_string(),
        session_id: if session_id.is_empty() {
            None
        } else {
            Some(session_id.to_string())
        },
        session_name: Some(session_name.to_string()),
        from_user: Some(from_user.to_string()),
        cwd: None,
        profile_dir: None,
        timeout_secs: Some(profile.timeout_secs),
        streaming: Some(profile.streaming),
        extra_env: std::collections::HashMap::new(),
        attachments: attachments.to_vec(),
        on_partial: Some(on_partial),
        on_session: None,
        on_error: None,
        on_permission: profile.permission.then_some(on_permission),
        on_stderr: None,
    };

    let result = run(&ap_profile, opts)
        .await
        .with_context(|| format!("agentproc run for profile `{profile_name}`"))?;

    let cli_session = if result.session_id.is_empty() {
        None
    } else {
        Some(result.session_id)
    };
    let error_event = !result.error.is_empty();
    let body = if error_event {
        // agentproc surfaced an error; the error text was forwarded via
        // on_partial-equivalent path is NOT available here, so surface it
        // as the body. handle.rs treats error_event turns distinctly.
        result.error.clone()
    } else {
        result.reply
    };

    let summary = RunSummary {
        duration_ms: result.duration_ms,
        exit_code: if result.exit_code == 0 {
            None
        } else {
            Some(result.exit_code)
        },
        partial_count: partial_count.load(Ordering::Relaxed),
        body_bytes: body.len(),
        cli_session_present: cli_session.is_some(),
        error_event,
        usage: result.usage,
    };

    if error_event {
        anyhow::bail!("{body}");
    }

    Ok((body, cli_session, summary))
}

// ─── `ask` permission strategy helpers ───────────────────────────────────
// Re-implemented here (the old copies in executor.rs will be deleted in the
// cleanup task). The WeChat prompt format and reply parsing are identical.

/// Build the WeChat-facing prompt for a permission_request.
fn format_approval_question(req: &agentproc::PermissionRequest) -> String {
    let input_preview = pretty_input(&req.input, 400);
    format!(
        "🔧 工具「{}」请求授权\n{}\n\n回复「允许」或「拒绝」",
        req.tool_name, input_preview
    )
}

fn pretty_input(input: &serde_json::Value, max_chars: usize) -> String {
    let pretty = if input.is_object() || input.is_array() {
        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
    } else {
        input.to_string()
    };
    if pretty.chars().count() <= max_chars {
        pretty
    } else {
        let truncated: String = pretty.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// Parse a WeChat reply into an allow/deny decision. Recognises common
/// Chinese and English affirmations/negations.
fn parse_approval_reply(text: &str) -> Option<bool> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    match t.as_str() {
        "允许" | "同意" | "好" | "yes" | "y" | "ok" | "allow" => Some(true),
        "拒绝" | "不行" | "no" | "n" | "deny" => Some(false),
        _ => None,
    }
}

/// Await the user's WeChat reply on the approval inbox. Reprompts up to
/// MAX_REPROMPTS times on unrecognised text; denies on timeout or channel close.
async fn await_user_approval(
    inbox: &mut tokio::sync::mpsc::Receiver<WeixinMessage>,
    request_id: &str,
    tool_name: &str,
    timeout: Duration,
    partial_tx: &watch::Sender<Option<String>>,
) -> PermissionDecision {
    const MAX_REPROMPTS: u32 = 2;
    let mut unrecognized: u32 = 0;
    loop {
        match tokio::time::timeout(timeout, inbox.recv()).await {
            Err(_) => {
                let _ =
                    partial_tx.send(Some(format!("⏱️ 工具「{tool_name}」授权超时，已自动拒绝")));
                return PermissionDecision::deny("approval timed out (no user reply)");
            }
            Ok(None) => {
                return PermissionDecision::deny("approval channel closed");
            }
            Ok(Some(msg)) => {
                let text = msg.text().unwrap_or("");
                match parse_approval_reply(text) {
                    Some(true) => {
                        let _ = partial_tx.send(Some(format!("✅ 已允许工具「{tool_name}」")));
                        let _ = request_id; // silence unused in allow path
                        return PermissionDecision::allow();
                    }
                    Some(false) => {
                        let _ = partial_tx.send(Some(format!("🚫 已拒绝工具「{tool_name}」")));
                        return PermissionDecision::deny("denied by user");
                    }
                    None => {
                        unrecognized += 1;
                        if unrecognized >= MAX_REPROMPTS {
                            let _ = partial_tx.send(Some(format!(
                                "未识别回复「{text}」，已按拒绝处理工具「{tool_name}」"
                            )));
                            return PermissionDecision::deny("unrecognized approval reply");
                        }
                        let _ = partial_tx.send(Some(format!(
                            "未识别回复「{text}」，请回复「允许」或「拒绝」"
                        )));
                        continue;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_approval_reply_recognises_tokens() {
        assert_eq!(parse_approval_reply("允许"), Some(true));
        assert_eq!(parse_approval_reply(" yes "), Some(true));
        assert_eq!(parse_approval_reply("拒绝"), Some(false));
        assert_eq!(parse_approval_reply("NO"), Some(false));
        assert_eq!(parse_approval_reply(""), None);
        assert_eq!(parse_approval_reply("maybe"), None);
    }

    #[test]
    fn format_approval_question_includes_tool_and_input() {
        let req = agentproc::PermissionRequest {
            request_id: "1".into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            description: None,
            tool_use_id: None,
            session_id: None,
        };
        let q = format_approval_question(&req);
        assert!(q.contains("Bash"));
        assert!(q.contains("ls"));
        assert!(q.contains("允许"));
    }

    #[test]
    fn executor_name_for_type_maps_known_types() {
        assert_eq!(
            executor_name_for_type("claude-code"),
            Some("claude-code".into())
        );
        assert_eq!(executor_name_for_type("codex"), Some("codex".into()));
        // codebuddy-code type maps to agentproc's `codebuddy` executor name.
        assert_eq!(
            executor_name_for_type("codebuddy-code"),
            Some("codebuddy".into())
        );
        assert_eq!(executor_name_for_type("recursive"), None);
        assert_eq!(executor_name_for_type("custom"), None);
    }

    #[test]
    fn to_agentproc_profile_preserves_fields() {
        let bp = BridgeProfile {
            command: "./agent".into(),
            args: vec!["{{MESSAGE}}".into()],
            timeout_secs: 42,
            streaming: false,
            permission: true,
            profile_type: Some("codex".into()),
            ..Default::default()
        };
        let ap = to_agentproc_profile(&bp);
        assert_eq!(ap.command, "./agent");
        assert_eq!(ap.timeout_secs, 42);
        assert!(!ap.streaming);
        assert!(ap.permission);
        assert_eq!(ap.executor.as_deref(), Some("codex"));
    }
}
