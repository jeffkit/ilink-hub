//! Bridge between ilink-hub's dispatcher and `agentproc::run`.
//!
//! This module replaces the old `executor::run_cli` call path: instead of
//! spawning `ilink-hub-bridge profile <type>` as a subprocess and speaking
//! AgentProc over its stdin/stdout, the dispatcher now drives `agentproc::run`
//! directly — either in-process (when the profile sets a registered `executor`
//! like `claude-code`) or via spawn (for custom `command:` profiles).
//!
//! ilink-hub-specific concerns stay here:
//! - `on_permission` — auto-allows every tool request (no per-profile policy).
//!   The WeChat approval UX can be reintroduced later by routing
//!   `permission_request` events to an approval broker.
//! - partial forwarding via `watch::Sender<Option<String>>` (streaming to WeChat).
//! - `CliRunSummary`-compatible metrics for the handle.rs logging/dedup path.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::info;

use agentproc::{
    run, Attachment as ApAttachment, PermissionDecision, PermissionFuture, Profile as ApProfile,
    RunOptions,
};

use crate::bridge::config::BridgeProfile;

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
/// `executor:` (an agentproc-spec field) selects the in-process executor when
/// one is registered (e.g. `claude-code`). Custom `command:` / `script:`
/// profiles fall through to the spawn path. Every spec field — including
/// `send_error_reply` — passes through verbatim; ilink-hub does not override
/// agentproc behaviour at a higher level.
pub(super) fn to_agentproc_profile(p: &BridgeProfile) -> ApProfile {
    let executor = p.executor.clone();
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
        send_error_reply: p.send_error_reply,
        streaming: p.streaming,
        permission: p.permission,
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

    // on_permission: ilink-hub no longer layers a per-profile policy on top of
    // agentproc's permission channel. When `permission: true` enables the
    // channel, every tool request is auto-allowed (equivalent to skip-
    // permissions, but routed through the protocol). The WeChat approval UX can
    // be reintroduced later by wiring `permission_request` events to an
    // approval broker.
    let profile_name_for_perm = profile_name.to_string();
    let on_permission = Arc::new(
        move |req: agentproc::PermissionRequest| -> PermissionFuture {
            let profile_name = profile_name_for_perm.clone();
            Box::pin(async move {
                info!(
                    profile = %profile_name,
                    request_id = %req.request_id,
                    tool = %req.tool_name,
                    "permission auto-allowed (no per-profile policy)"
                );
                PermissionDecision::allow()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_agentproc_profile_preserves_fields() {
        let bp = BridgeProfile {
            command: "./agent".into(),
            args: vec!["{{MESSAGE}}".into()],
            timeout_secs: 42,
            streaming: false,
            permission: true,
            send_error_reply: false,
            executor: Some("codex".into()),
            ..Default::default()
        };
        let ap = to_agentproc_profile(&bp);
        assert_eq!(ap.command, "./agent");
        assert_eq!(ap.timeout_secs, 42);
        assert!(!ap.streaming);
        assert!(ap.permission);
        assert!(!ap.send_error_reply);
        assert_eq!(ap.executor.as_deref(), Some("codex"));
    }
}
