//! AgentProc 0.4 bridge-side executor: spawn the agent process, write the NDJSON
//! turn object to its stdin, read NDJSON events from its stdout (`partial` /
//! `result` / `error` / `permission_request`), and forward partials / assemble
//! the final reply body.
//!
//! See `docs/knowledge/bridges/profile-protocol.md` and the upstream spec at
//! `~/projects/agentproc/spec/protocol.md`.
//
// MIGRATION NOTE: the dispatcher now drives `agentproc::run` via
// `dispatcher/agentproc_runner`. The functions below (run_cli, CliRunSummary,
// apply_placeholders, permission helpers, env expansion) are dead code that
// will be deleted in the cleanup task. Kept compiling via this allow so the
// migration lands in reviewable steps. Only build_attachments /
// split_into_parts / MAX_CLI_CAPTURE_BYTES are still live.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::bridge::config::BridgeProfile;
use crate::bridge::protocol::{self, AgentEvent, Attachment, PermissionResponse, TurnObject};
use crate::bridge::wire_assemble::is_valid_session_id;
use crate::bridge::ApprovalBroker;
use crate::ilink::types::WeixinMessage;
use crate::paths::expand_user_path;

/// Hard upper bound on how many bytes of a child's stdout/stderr we buffer in
/// memory before truncating. A misbehaving or malicious CLI could otherwise
/// stream unbounded output and OOM the Hub. This is purely a safety valve: the
/// final reply is separately truncated to `max_reply_chars` (default 8000), so
/// this cap is ~8000× any legitimate reply and never triggers in normal use.
pub const MAX_CLI_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// Replace `{{MESSAGE}}`, `{{SESSION_ID}}`, `{{SESSION_NAME}}`, and
/// `{{PROFILE_DIR}}` in a template string.
///
/// SEC-003: `message` is user-controlled (forwarded WeChat message text). We
/// refuse to inject any string that contains bytes which would be interpreted
/// by a shell-style wrapper (`bash -c`, `sh -c`, `env` parsing) — NUL,
/// newlines, or carriage returns. Only validates a field when its placeholder
/// actually appears in the template; callers that deliver the message via the
/// stdin turn object (the 0.4 default) will not have `{{MESSAGE}}` in any arg
/// template and must not be rejected just because the message contains
/// newlines.
///
/// `{{PROFILE_DIR}}` expands to the profile file's directory (empty when the
/// profile was built programmatically without a file, per the spec).
pub(super) fn apply_placeholders(
    template: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
    profile_dir: &str,
) -> Result<String, PlaceholderError> {
    if template.contains("{{MESSAGE}}") {
        validate_safe_value("message", message)?;
    }
    if template.contains("{{SESSION_ID}}") {
        validate_safe_value("session_id", session_id)?;
    }
    if template.contains("{{SESSION_NAME}}") {
        validate_safe_value("session_name", session_name)?;
    }
    Ok(template
        .replace("{{MESSAGE}}", message)
        .replace("{{SESSION_ID}}", session_id)
        .replace("{{SESSION_NAME}}", session_name)
        .replace("{{PROFILE_DIR}}", profile_dir))
}

/// Reject values that contain characters unsafe for shell-style wrappers.
fn validate_safe_value(field: &str, value: &str) -> Result<(), PlaceholderError> {
    for b in value.bytes() {
        if b == 0 || b == b'\n' || b == b'\r' {
            return Err(PlaceholderError::UnsafeValue {
                field: field.to_string(),
            });
        }
    }
    Ok(())
}

/// Sanitize a value destined for a subprocess environment variable by stripping
/// NUL, CR, and LF bytes. These characters can cause env-var truncation or
/// argument-injection in shell wrappers. When the value is dirty a WARN is
/// logged and an empty string is returned so message processing is not aborted.
fn sanitize_env_value(field: &str, value: &str) -> String {
    let mut has_nul = false;
    let mut has_newline = false;
    let mut sanitized = String::with_capacity(value.len());

    for c in value.chars() {
        if c == '\0' {
            has_nul = true;
        } else if c == '\n' || c == '\r' {
            has_newline = true;
            sanitized.push(' ');
        } else {
            sanitized.push(c);
        }
    }

    if has_nul || has_newline {
        warn!(
            field = %field,
            has_nul = %has_nul,
            has_newline = %has_newline,
            "bridge env var value contains NUL/CR/LF control character; NUL removed, CR/LF replaced by space (SEC-011)"
        );
    }

    sanitized
}

#[derive(Debug, thiserror::Error)]
pub enum PlaceholderError {
    #[error("placeholder value for `{field}` contains NUL/newline; refusing to inject")]
    UnsafeValue { field: String },
}

/// Build the `attachments` array for the turn object from a WeChat message's
/// media items. Replaces the 0.2 `AGENT_IMAGE_URL` / `AGENT_FILE_URL` /
/// `AGENT_VIDEO_URL` env vars — under 0.4 all media travels in the turn object.
pub(super) fn build_attachments(msg: &WeixinMessage) -> Vec<Attachment> {
    use crate::ilink::types::msg_type;
    let mut out = Vec::new();
    let Some(items) = msg.item_list.as_ref() else {
        return out;
    };
    for item in items.iter() {
        match item.item_type {
            Some(msg_type::IMAGE) => {
                if let Some(url) = item
                    .image_item
                    .as_ref()
                    .and_then(|i| i.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(Attachment {
                        kind: "image".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            Some(msg_type::FILE) => {
                if let Some(fi) = item.file_item.as_ref() {
                    if let Some(url) = fi.cdn_url.as_deref().filter(|s| !s.is_empty()) {
                        out.push(Attachment {
                            kind: "file".into(),
                            url: url.to_string(),
                            filename: fi.file_name.as_deref().map(|s| s.to_string()),
                            mime_type: None,
                            size: None,
                        });
                    }
                }
                break;
            }
            Some(msg_type::VIDEO) => {
                if let Some(url) = item
                    .video_item
                    .as_ref()
                    .and_then(|v| v.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(Attachment {
                        kind: "video".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            _ => {}
        }
    }
    out
}

/// Metrics collected for one `run_cli` invocation (for structured logging).
#[derive(Debug, Clone)]
pub(super) struct CliRunSummary {
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub exited_by_signal: bool,
    /// Number of `partial` events actually forwarded to the user (0 when
    /// `streaming: false` — the bridge ignores partials in that mode).
    pub partial_count: u32,
    pub body_bytes: usize,
    pub stderr_bytes: usize,
    pub cli_session_present: bool,
    /// True when the agent emitted an `error` event (turn is failed regardless
    /// of exit code; the error text was already forwarded to the user).
    pub error_event: bool,
    /// Optional AgentProc 0.4 `usage` object from `result` / `error` (MAY ignore
    /// unknown keys). Surfaced in logs and forwarded to Hub via `HubExt`.
    pub usage: Option<serde_json::Value>,
}

fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated, {} bytes total]", &s[..end], s.len())
}

/// Compose the fixed infra set the child always inherits (per agentproc 0.4).
/// None of these are credential-bearing; they let the agent find its
/// interpreter, temp dir, and locale.
fn infra_env_vars() -> &'static [&'static str] {
    &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "TERM",
        "TMPDIR",
        "TZ",
        "PWD",
    ]
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_cli(
    cfg: &BridgeProfile,
    profile_name: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
    from_user: &str,
    context_token: &str,
    attachments: &[Attachment],
    partial_tx: watch::Sender<Option<String>>,
    approval_broker: Arc<ApprovalBroker>,
    session_key: String,
) -> Result<(String, Option<String>, CliRunSummary)> {
    let started = Instant::now();
    // {{PROFILE_DIR}} is not yet wired to the profile file's directory; expand
    // to empty (spec-compliant when the profile is used without a file path).
    const PROFILE_DIR: &str = "";
    let args: Vec<String> = cfg
        .args
        .iter()
        .map(|a| {
            apply_placeholders(a, message, session_id, session_name, PROFILE_DIR)
                .with_context(|| format!("unsafe placeholder value in args template `{a}`"))
        })
        .collect::<Result<_>>()?;

    let command = super::paths::resolve_spawn_command(&cfg.command);

    let mut cmd = Command::new(&command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &cfg.cwd {
        let dir = expand_user_path(
            &apply_placeholders(dir, message, session_id, session_name, PROFILE_DIR)
                .with_context(|| format!("unsafe placeholder value in cwd template `{dir}`"))?,
        );
        cmd.current_dir(&dir);
    } else if let Some(home) = dirs::home_dir() {
        cmd.current_dir(&home);
    }

    // Infra set: always copied from the bridge's own environment (0.4 removes
    // the `env_inherit` escape hatch; ambient vars a profile needs must be
    // declared in its `env` block).
    let bridge_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    for name in infra_env_vars() {
        if let Some(val) = bridge_env.get(*name) {
            cmd.env(name, val);
        }
    }

    // AGENT_CONTEXT_TOKEN is an ilink-hub extension (not part of the agentproc
    // turn object) carried in the env so external profiles that call back to
    // the Hub can read it. No built-in profile consumes it.
    cmd.env(
        "AGENT_CONTEXT_TOKEN",
        sanitize_env_value("AGENT_CONTEXT_TOKEN", context_token),
    );

    for (k, v) in &cfg.env {
        let v = apply_placeholders(v, message, session_id, session_name, PROFILE_DIR)
            .with_context(|| format!("unsafe placeholder value in env var `{k}`"))?;
        let v = crate::bridge::config::expand_env_var_named_with_allowlist(
            &v,
            &bridge_env,
            cfg.env_allowlist.as_deref(),
            Some(profile_name),
            Some(&format!("env.{k}")),
        )
        .with_context(|| format!("expand env var `{k}` for profile `{profile_name}`"))?;
        cmd.env(k, v);
    }

    // stdin always carries the NDJSON turn object.
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{command}`"))?;

    let pid = child.id();
    info!(
        profile = profile_name,
        session_name = session_name,
        pid = pid,
        command = %command,
        args = ?args,
        streaming = cfg.streaming,
        permission = cfg.permission,
        timeout_secs = cfg.timeout_secs,
        "CLI spawned"
    );

    let dur = Duration::from_secs(cfg.timeout_secs.max(1));

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        tokio::io::BufReader::new(child_stderr)
            .take(MAX_CLI_CAPTURE_BYTES as u64)
            .read_to_end(&mut buf)
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Build the turn object and write it as the first stdin line. When
    // `permission: true`, stdin stays open for permission_response frames.
    let turn = TurnObject::new(
        message,
        session_id,
        if session_name.is_empty() {
            "default"
        } else {
            session_name
        },
        from_user,
        attachments.to_vec(),
        cfg.permission,
    );
    let turn_line = turn.to_ndjson().context("serialize turn object")?;
    let permission_enabled = cfg.permission;

    let (perm_tx, mut perm_rx) = mpsc::channel::<PermissionResponse>(16);
    let mut child_stdin = child.stdin.take().context("stdin pipe missing")?;
    let stdin_task: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
        child_stdin
            .write_all(turn_line.as_bytes())
            .await
            .context("write turn object to stdin")?;
        child_stdin
            .write_all(b"\n")
            .await
            .context("write turn newline")?;
        if !permission_enabled {
            child_stdin.shutdown().await.context("shutdown stdin")?;
            return Ok(());
        }
        // Keep stdin open; forward permission_response frames as they arrive.
        while let Some(resp) = perm_rx.recv().await {
            let line = resp.to_ndjson().context("serialize permission response")?;
            if child_stdin.write_all(line.as_bytes()).await.is_err() {
                // Child closed stdin; stop writing.
                break;
            }
            if child_stdin.write_all(b"\n").await.is_err() {
                break;
            }
            child_stdin.flush().await.ok();
        }
        // No more responses coming; close stdin so the agent can finalize.
        let _ = child_stdin.shutdown().await;
        Ok(())
    });

    let streaming = cfg.streaming;
    let max_chars = cfg.max_reply_chars;
    let truncation_suffix = cfg.truncation_suffix.clone();
    let truncation_suffix_for_body = truncation_suffix.clone();
    let permission_default = cfg.permission_default;
    let ask_timeout = Duration::from_secs(cfg.permission_ask_timeout_secs.max(1));
    let approval_broker = Some(approval_broker);

    /// Output of the stdout-parsing loop: text chunks, session id, forwarded
    /// partial count, error-event flag, and the error message (for non-streaming
    /// final body). Factored into a type alias to keep the closure signature readable.
    type StreamOutcome = (
        Vec<String>,
        Option<String>,
        u32,
        bool,
        Option<String>,
        Option<serde_json::Value>,
    );

    let stream_result: Result<StreamOutcome> = tokio::time::timeout(dur, async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(child_stdout);
        let mut text_chunks: Vec<String> = Vec::new();
        let mut text_bytes: usize = 0;
        let mut session_id: Option<String> = None;
        let mut partial_count: u32 = 0;
        let mut cumulative_partial_chars: usize = 0;
        let mut partials_truncated = false;
        let mut error_seen = false;
        let mut error_message: Option<String> = None;
        let mut usage: Option<serde_json::Value> = None;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.context("read stdout")?;
            if n == 0 {
                break;
            }
            let Some(event) = protocol::parse_event(&line) else {
                // Malformed / unknown line: log and ignore (not body, per 0.4).
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if !trimmed.is_empty() {
                    warn!(
                        profile = profile_name,
                        raw = %truncate_for_log(trimmed, 512),
                        "ignoring non-NDJSON stdout line (not a recognised event)"
                    );
                }
                continue;
            };
            // 0.4: session continuity is an optional field on events; first
            // non-empty wins. A later conflicting value is a violation — keep
            // the first and warn.
            if let Some(sid) = event.session_id() {
                if !is_valid_session_id(sid) {
                    warn!(
                        profile = profile_name,
                        session_id = %sid,
                        "ignoring invalid session_id (path separators/control chars)"
                    );
                } else {
                    match &session_id {
                        None => session_id = Some(sid.to_string()),
                        Some(existing) if existing != sid => {
                            warn!(
                                profile = profile_name,
                                kept = %existing,
                                ignored = %sid,
                                "conflicting session_id on event; keeping first"
                            );
                        }
                        Some(_) => {}
                    }
                }
            }
            match event {
                AgentEvent::Partial { text, .. } => {
                    if error_seen || !streaming || partials_truncated || text.is_empty() {
                        continue;
                    }
                    let remaining = max_chars.saturating_sub(cumulative_partial_chars);
                    if remaining == 0 {
                        let _ = partial_tx.send(Some(truncation_suffix.clone()));
                        partials_truncated = true;
                        continue;
                    }
                    if text.chars().count() > remaining {
                        let chunk: String = text.chars().take(remaining).collect();
                        partial_count = partial_count.saturating_add(1);
                        let _ = partial_tx.send(Some(chunk));
                        let _ = partial_tx.send(Some(truncation_suffix.clone()));
                        partials_truncated = true;
                        cumulative_partial_chars = max_chars;
                    } else {
                        partial_count = partial_count.saturating_add(1);
                        cumulative_partial_chars += text.chars().count();
                        let _ = partial_tx.send(Some(text));
                    }
                }
                AgentEvent::Result {
                    text,
                    usage: event_usage,
                    ..
                } => {
                    if usage.is_none() {
                        usage = event_usage;
                    }
                    if error_seen {
                        continue;
                    }
                    // At most one result per turn; subsequent ones are ignored.
                    if !text_chunks.is_empty() {
                        warn!(
                            profile = profile_name,
                            "ignoring subsequent result event (at most one per turn)"
                        );
                        continue;
                    }
                    if text_bytes + text.len() > MAX_CLI_CAPTURE_BYTES {
                        let room = MAX_CLI_CAPTURE_BYTES - text_bytes;
                        let mut added = 0usize;
                        let chunk: String = text
                            .chars()
                            .take_while(|c| {
                                let len = c.len_utf8();
                                if added + len > room {
                                    return false;
                                }
                                added += len;
                                true
                            })
                            .collect();
                        if !chunk.is_empty() {
                            text_chunks.push(chunk);
                            text_bytes = MAX_CLI_CAPTURE_BYTES;
                        }
                        warn!(
                            limit_bytes = MAX_CLI_CAPTURE_BYTES,
                            "CLI result body exceeded capture limit; hard-truncating"
                        );
                    } else {
                        text_bytes += text.len();
                        text_chunks.push(text);
                    }
                }
                AgentEvent::Error {
                    message,
                    usage: event_usage,
                    ..
                } => {
                    if usage.is_none() {
                        usage = event_usage;
                    }
                    if !message.trim().is_empty() {
                        error_message = Some(message.clone());
                    }
                    error_seen = true;
                    // Forward the error text to the user immediately (streaming)
                    // or as the final body (non-streaming).
                    if streaming {
                        let _ = partial_tx.send(Some(message));
                    }
                }
                AgentEvent::PermissionRequest(req) => {
                    if !permission_enabled {
                        // Profile didn't opt in; ignore (spec says log + don't block).
                        warn!(
                            profile = profile_name,
                            request_id = %req.request_id,
                            tool = %req.tool_name,
                            "ignoring permission_request (profile.permission is false)"
                        );
                        continue;
                    }
                    let resp = match permission_default {
                        protocol::PermissionDefaultPolicy::Allow => {
                            info!(
                                profile = profile_name,
                                request_id = %req.request_id,
                                tool = %req.tool_name,
                                "permission auto-allowed (permission_default)"
                            );
                            PermissionResponse::allow(req.request_id)
                        }
                        protocol::PermissionDefaultPolicy::Deny
                        | protocol::PermissionDefaultPolicy::DenyLogged => {
                            warn!(
                                profile = profile_name,
                                request_id = %req.request_id,
                                tool = %req.tool_name,
                                input = ?req.input,
                                "permission denied (permission_default)"
                            );
                            PermissionResponse::deny(
                                req.request_id.clone(),
                                "denied by bridge permission_default policy",
                            )
                        }
                        protocol::PermissionDefaultPolicy::Ask => {
                            // Interactive approval: pause the turn, prompt the
                            // user over WeChat via the partial channel, and
                            // await their reply on the same session (routed
                            // back by the ApprovalBroker). Falls back to deny
                            // when no broker is wired (tests / probe).
                            match approval_broker.as_ref() {
                                None => {
                                    warn!(
                                        profile = profile_name,
                                        request_id = %req.request_id,
                                        tool = %req.tool_name,
                                        "permission ask: no approval broker; denying"
                                    );
                                    PermissionResponse::deny(
                                        req.request_id.clone(),
                                        "ask policy unavailable (no approval broker)",
                                    )
                                }
                                Some(broker) => {
                                    let question = format_approval_question(&req);
                                    // Register the inbox BEFORE sending the
                                    // question so the user's reply cannot
                                    // arrive before we listen.
                                    let (mut inbox, guard) = broker.register(session_key.clone());
                                    let _ = partial_tx.send(Some(question));
                                    info!(
                                        profile = profile_name,
                                        request_id = %req.request_id,
                                        tool = %req.tool_name,
                                        timeout_secs = ask_timeout.as_secs(),
                                        "permission ask: prompting user"
                                    );
                                    let resp = await_user_approval(
                                        &mut inbox,
                                        &req.request_id,
                                        &req.tool_name,
                                        ask_timeout,
                                        &partial_tx,
                                    )
                                    .await;
                                    drop(guard);
                                    resp
                                }
                            }
                        }
                    };
                    // Send to the stdin writer task. If the channel is closed
                    // (child died / stdin gone) the response is dropped — the
                    // agent is expected to exit shortly.
                    let _ = perm_tx.send(resp).await;
                }
            }
        }
        drop(partial_tx);
        drop(perm_tx);
        Ok((
            text_chunks,
            session_id,
            partial_count,
            error_seen,
            error_message,
            usage,
        ))
    })
    .await
    .map_err(|_| anyhow::anyhow!("CLI timed out after {}s", cfg.timeout_secs))?;

    let (text_chunks, cli_sid, partial_count, error_seen, error_message, usage) = stream_result?;

    // The stdin task may still be alive if it is blocked sending a permission
    // response; closing perm_tx above unblocks it. Wait briefly for it to flush.
    if let Err(e) = tokio::time::timeout(Duration::from_secs(2), stdin_task)
        .await
        .map_err(|_| anyhow::anyhow!("stdin task timed out"))
    {
        warn!(error = ?e, "stdin task joined with error (non-fatal)");
    }

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| anyhow::anyhow!("CLI failed to exit after stdout EOF"))?
        .context("wait for CLI process")?;

    let stderr = match stderr_task.await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "CLI stderr task joined with error");
            String::new()
        }
    };
    if !stderr.is_empty() {
        warn!(
            profile = profile_name,
            session_name = session_name,
            pid = pid,
            stderr = %truncate_for_log(&stderr, 4096),
            "CLI stderr"
        );
    }

    let exit_code = status.code();
    let exited_by_signal = !status.success() && exit_code.is_none();
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    // Assemble the final body from the result event. In non-streaming mode this
    // is what the user receives; in streaming mode the dispatcher drops it when
    // partials were already forwarded (A1 dedup policy).
    let mut body = text_chunks.concat();
    if !body.is_empty() && body.chars().count() > max_chars {
        let truncated: String = body.chars().take(max_chars).collect();
        body = format!("{truncated}{truncation_suffix_for_body}");
    }

    // Error event: the turn is failed. The error text was already forwarded
    // (streaming) or becomes the final body (non-streaming). Persist the
    // session anyway (the error terminates the turn, not the session).
    if error_seen {
        let err_text = error_message.unwrap_or_default();
        let body_out = if streaming { String::new() } else { err_text };
        let cli_session_present = cli_sid.as_ref().is_some_and(|s| !s.is_empty());
        let summary = CliRunSummary {
            duration_ms,
            exit_code,
            exited_by_signal,
            partial_count,
            body_bytes: body_out.len(),
            stderr_bytes: stderr.len(),
            cli_session_present,
            error_event: true,
            usage: usage.clone(),
        };
        info!(
            profile = profile_name,
            session_name = session_name,
            pid = pid,
            duration_ms = summary.duration_ms,
            exit_code = ?summary.exit_code,
            partial_count = summary.partial_count,
            cli_session = summary.cli_session_present,
            usage = ?summary.usage,
            "CLI finished: error event"
        );
        return Ok((body_out, cli_sid, summary));
    }

    if cfg.include_stderr_in_reply && !stderr.is_empty() {
        body.push_str("\n--- stderr ---\n");
        body.push_str(&stderr);
    }

    let cli_session_present = cli_sid.as_ref().is_some_and(|s| !s.is_empty());
    let summary = CliRunSummary {
        duration_ms,
        exit_code,
        exited_by_signal,
        partial_count,
        body_bytes: body.len(),
        stderr_bytes: stderr.len(),
        cli_session_present,
        error_event: false,
        usage: usage.clone(),
    };

    info!(
        profile = profile_name,
        session_name = session_name,
        pid = pid,
        duration_ms = summary.duration_ms,
        exit_code = ?summary.exit_code,
        exited_by_signal = summary.exited_by_signal,
        partial_count = summary.partial_count,
        body_bytes = summary.body_bytes,
        stderr_bytes = summary.stderr_bytes,
        cli_session = summary.cli_session_present,
        usage = ?summary.usage,
        success = status.success(),
        "CLI finished"
    );

    // Exit-code precedence: timeout (handled above) > error event (handled
    // above) > process exit code. A non-zero exit is tolerated when we
    // recovered a session id or any body text (matches the legacy
    // `ensure_success(recovered=...)` behaviour).
    if !status.success() && cli_sid.is_none() && body.trim().is_empty() {
        let code = exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        anyhow::bail!(
            "command exited with status {code}\n--- stderr ---\n{stderr}\n--- stdout ---\n{body}"
        );
    }

    Ok((body, cli_sid, summary))
}

/// Split `s` into a sequence of parts, each at most `max_chars` Unicode chars.
/// Returns at least one element (possibly an empty string when `s` is empty).
pub(super) fn split_into_parts(s: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![s.to_string()];
    }
    let mut parts = Vec::new();
    let mut chars = s.chars().peekable();
    while chars.peek().is_some() {
        let part: String = chars.by_ref().take(max_chars).collect();
        parts.push(part);
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

// ─── `ask` permission strategy helpers ───────────────────────────────────────

/// Build the WeChat-facing prompt for a `permission_request`. The tool input
/// is pretty-printed and capped so a huge command/object doesn't blow up the
/// message.
fn format_approval_question(req: &protocol::PermissionRequest) -> String {
    const MAX_INPUT_CHARS: usize = 800;
    let input_preview = {
        let raw = if req.input.is_null() {
            String::new()
        } else {
            serde_json::to_string_pretty(&req.input).unwrap_or_else(|_| req.input.to_string())
        };
        let raw = raw.trim();
        if raw.chars().count() <= MAX_INPUT_CHARS {
            raw.to_string()
        } else {
            let truncated: String = raw.chars().take(MAX_INPUT_CHARS).collect();
            format!("{truncated}\n…(输入已截断)")
        }
    };
    let tool = req.tool_name.trim();
    if input_preview.is_empty() {
        format!("🔧 工具「{tool}」请求授权\n回复「允许」或「拒绝」")
    } else {
        format!("🔧 工具「{tool}」请求授权：\n{input_preview}\n\n回复「允许」或「拒绝」")
    }
}

/// Parse a user reply into an allow/deny decision.
///
/// Loose matching (Q2=A) over a fixed token set: 允许/yes/y/是/ok/好/同意/1 →
/// allow; 拒绝/no/n/否/0/deny/取消 → deny; anything else → `None` (caller
/// reprompts). Matching is on the trimmed, lower-cased reply so "Yes", " OK "
/// and "允许" all work, but a free-form sentence won't misfire on a stray
/// single letter.
fn parse_approval_reply(text: &str) -> Option<bool> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    const ALLOW: &[&str] = &[
        "允许", "yes", "y", "是", "ok", "好", "同意", "1", "allow", "approve", "许可",
    ];
    const DENY: &[&str] = &[
        "拒绝", "no", "n", "否", "0", "deny", "reject", "取消", "cancel", "不行",
    ];
    if ALLOW.contains(&t.as_str()) {
        return Some(true);
    }
    if DENY.contains(&t.as_str()) {
        return Some(false);
    }
    None
}

/// Await the user's allow/deny reply on the approval inbox.
///
/// Loose matching with up to 2 unrecognized reprompts before denying (Q2=A,
/// Q3=A: any inbound message during the ask window is treated as the reply).
/// On timeout the tool is denied and the user is notified (Q1=C).
async fn await_user_approval(
    inbox: &mut mpsc::Receiver<WeixinMessage>,
    request_id: &str,
    tool_name: &str,
    timeout: Duration,
    partial_tx: &watch::Sender<Option<String>>,
) -> PermissionResponse {
    const MAX_REPROMPTS: u32 = 2;
    let mut unrecognized: u32 = 0;
    loop {
        match tokio::time::timeout(timeout, inbox.recv()).await {
            Err(_) => {
                let _ =
                    partial_tx.send(Some(format!("⏱️ 工具「{tool_name}」授权超时，已自动拒绝")));
                return PermissionResponse::deny(
                    request_id.to_string(),
                    "approval timed out (no user reply)",
                );
            }
            Ok(None) => {
                // Inbox closed (broker dropped / bridge shutdown). Deny quietly.
                return PermissionResponse::deny(request_id.to_string(), "approval channel closed");
            }
            Ok(Some(msg)) => {
                let text = msg.text().unwrap_or("");
                match parse_approval_reply(text) {
                    Some(true) => {
                        let _ = partial_tx.send(Some(format!("✅ 已允许工具「{tool_name}」")));
                        return PermissionResponse::allow(request_id.to_string());
                    }
                    Some(false) => {
                        let _ = partial_tx.send(Some(format!("🚫 已拒绝工具「{tool_name}」")));
                        return PermissionResponse::deny(request_id.to_string(), "denied by user");
                    }
                    None => {
                        unrecognized += 1;
                        if unrecognized >= MAX_REPROMPTS {
                            let _ = partial_tx.send(Some(format!(
                                "未识别回复「{text}」，已按拒绝处理工具「{tool_name}」"
                            )));
                            return PermissionResponse::deny(
                                request_id.to_string(),
                                "unrecognized approval reply",
                            );
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
    use crate::bridge::config::BridgeApp;
    use tokio::sync::watch;

    #[test]
    fn placeholders_message_session_id_and_name() {
        assert_eq!(
            apply_placeholders(
                "{{MESSAGE}}|{{SESSION_ID}}|{{SESSION_NAME}}|{{PROFILE_DIR}}",
                "hi",
                "sid-9",
                "feat-a",
                "/p"
            )
            .unwrap(),
            "hi|sid-9|feat-a|/p"
        );
    }

    #[test]
    fn placeholders_profile_dir_empty_when_unset() {
        assert_eq!(
            apply_placeholders("{{PROFILE_DIR}}/b.py", "hi", "sid", "name", "").unwrap(),
            "/b.py"
        );
    }

    #[test]
    fn placeholders_reject_nul_in_message() {
        let err =
            apply_placeholders("{{MESSAGE}}", "evil\0payload", "sid", "name", "").unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    /// Newline in message is OK when the template does not use {{MESSAGE}}
    /// (the 0.4 default — the message travels via the stdin turn object).
    #[test]
    fn placeholders_allow_newline_in_message_when_placeholder_absent() {
        let result = apply_placeholders(
            "--session={{SESSION_ID}}",
            "line1\nline2",
            "sid-1",
            "default",
            "",
        );
        assert!(
            result.is_ok(),
            "newline in message must be allowed when {{MESSAGE}} is not in template: {result:?}"
        );
    }

    #[test]
    fn placeholders_reject_newline_in_session_id() {
        let err = apply_placeholders("session={{SESSION_ID}}", "msg", "sid\nrm -rf /", "name", "")
            .unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    #[test]
    fn placeholders_reject_carriage_return_in_session_name() {
        let err = apply_placeholders("name={{SESSION_NAME}}", "msg", "sid", "feat\r\noops", "")
            .unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    #[test]
    fn placeholder_session_name_defaults_to_default() {
        assert_eq!(
            apply_placeholders("name={{SESSION_NAME}}", "", "", "default", "").unwrap(),
            "name=default"
        );
    }

    #[test]
    fn split_into_parts_basic() {
        let parts = split_into_parts("abcdefgh", 3);
        assert_eq!(parts, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn split_into_parts_exact() {
        let parts = split_into_parts("abcdef", 3);
        assert_eq!(parts, vec!["abc", "def"]);
    }

    #[test]
    fn split_into_parts_fits_in_one() {
        let parts = split_into_parts("hi", 10);
        assert_eq!(parts, vec!["hi"]);
    }

    #[test]
    fn split_into_parts_empty() {
        let parts = split_into_parts("", 8);
        assert_eq!(parts, vec![""]);
    }

    #[test]
    fn split_into_parts_unicode() {
        let parts = split_into_parts("一二三四五", 2);
        assert_eq!(parts, vec!["一二", "三四", "五"]);
    }

    #[test]
    fn test_sanitize_env_value_adversarial() {
        assert_eq!(sanitize_env_value("test", "hello"), "hello");
        assert_eq!(sanitize_env_value("test", "hello\nworld\r"), "hello world ");
        assert_eq!(sanitize_env_value("test", "hello\0world"), "helloworld");
        assert_eq!(
            sanitize_env_value("test", "hello\0\nworld\r"),
            "hello world "
        );
        assert_eq!(sanitize_env_value("test", "\0\0\0"), "");
        assert_eq!(sanitize_env_value("test", "\n\n\r\r"), "    ");
        assert_eq!(sanitize_env_value("test", "a\0b\nc\rd"), "ab c d");
    }

    /// Build a bridge YAML that runs `sh <temp-script>` with a piped stdin.
    /// The script is written to a temp file so YAML quoting of shell
    /// metacharacters is a non-issue.
    fn shell_profile_yaml(script: &str, timeout_secs: u64) -> String {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("create temp script");
        write!(tmp, "{script}").expect("write temp script");
        let path = tmp.into_temp_path().keep().expect("keep temp script");
        let path_str = path.to_string_lossy().to_string();
        format!("command: /bin/sh\nargs:\n  - \"{path_str}\"\ntimeout_secs: {timeout_secs}\n")
    }

    /// A 0.4 agent that reads the turn object from stdin and emits NDJSON
    /// events: partials (with session_id) and a final result body.
    #[tokio::test]
    async fn test_agentproc_partial_session_text_parsing() {
        // sh script: read stdin (the turn line), then emit NDJSON events.
        let script = r#"read -r line; \
printf '%s\n' '{"type":"partial","text":"chunk-a","session_id":"sess-xyz"}'; \
printf '%s\n' '{"type":"partial","text":"chunk-b","session_id":"sess-xyz"}'; \
printf '%s\n' '{"type":"result","text":"final-body","session_id":"sess-xyz"}'"#;
        let yaml = shell_profile_yaml(script, 5);
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);
        let chunks = spawn_partial_collector(partial_rx);

        let (body, cli_sid, summary) = run_cli(
            profile,
            "test_profile",
            "hello",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            ApprovalBroker::new(),
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        assert_eq!(cli_sid.as_deref(), Some("sess-xyz"));
        assert_eq!(body, "final-body");
        assert_eq!(summary.partial_count, 2);
        assert!(!summary.error_event);

        let chunks = chunks.await.expect("collector panicked");
        assert!(
            chunks.iter().any(|c| c == "chunk-b"),
            "expected chunk-b in forwarded partials, got {:?}",
            chunks
        );
    }

    /// Non-streaming mode: partials are ignored; the body comes from text events.
    #[tokio::test]
    async fn test_non_streaming_ignores_partials() {
        let script = r#"read -r line; \
printf '%s\n' '{"type":"partial","text":"live-chunk"}'; \
printf '%s\n' '{"type":"result","text":"assembled-body"}'"#;
        let yaml = format!("{}\nstreaming: false\n", shell_profile_yaml(script, 5));
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);
        let chunks = spawn_partial_collector(partial_rx);

        let (body, _sid, summary) = run_cli(
            profile,
            "test_profile",
            "hello",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            ApprovalBroker::new(),
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        assert_eq!(body, "assembled-body");
        assert_eq!(summary.partial_count, 0);

        let chunks = chunks.await.expect("collector panicked");
        assert!(
            chunks.is_empty(),
            "non-streaming must not forward partials, got {:?}",
            chunks
        );
    }

    /// An error event forwards the error text to the user (streaming) and
    /// produces an empty final body so the dispatcher skips the final send.
    #[tokio::test]
    async fn test_error_event_forwarded_as_partial() {
        let script =
            r#"read -r line; printf '%s\n' '{"type":"error","message":"boom: bad model"}'"#;
        let yaml = shell_profile_yaml(script, 5);
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);
        let chunks = spawn_partial_collector(partial_rx);

        let (body, _sid, summary) = run_cli(
            profile,
            "test_profile",
            "hello",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            ApprovalBroker::new(),
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        assert!(
            body.is_empty(),
            "streaming error → empty body, got: {body:?}"
        );
        assert!(summary.error_event);

        let chunks = chunks.await.expect("collector panicked");
        assert!(
            chunks.iter().any(|c| c == "boom: bad model"),
            "error text not forwarded as partial; got {:?}",
            chunks
        );
    }

    /// The turn object the bridge writes to stdin carries the message and
    /// protocol_version. Verified by an agent that echoes the parsed message.
    #[tokio::test]
    async fn test_turn_object_written_to_stdin() {
        // Use python3 to parse the JSON turn and emit it back as a text event.
        let script = r#"python3 -c 'import sys,json; t=json.loads(sys.stdin.readline()); print(json.dumps({"type":"result","text":"echo:"+t["message"]+"/"+t["protocol_version"]}))'"#;
        let yaml = shell_profile_yaml(script, 5);
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        let (partial_tx, _partial_rx) = watch::channel::<Option<String>>(None);
        let (body, _sid, _summary) = run_cli(
            profile,
            "test_profile",
            "hello",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            ApprovalBroker::new(),
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        assert_eq!(body, "echo:hello/0.4");
    }

    // ─── ask permission strategy ────────────────────────────────────────────

    #[test]
    fn parse_approval_reply_recognizes_tokens() {
        assert_eq!(parse_approval_reply("允许"), Some(true));
        assert_eq!(parse_approval_reply(" YES "), Some(true));
        assert_eq!(parse_approval_reply("ok"), Some(true));
        assert_eq!(parse_approval_reply("1"), Some(true));
        assert_eq!(parse_approval_reply("拒绝"), Some(false));
        assert_eq!(parse_approval_reply("no"), Some(false));
        assert_eq!(parse_approval_reply("0"), Some(false));
        assert_eq!(parse_approval_reply(""), None);
        assert_eq!(parse_approval_reply("maybe later"), None);
    }

    #[test]
    fn format_approval_question_includes_tool_and_input() {
        let req = protocol::PermissionRequest {
            request_id: "r1".into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "echo hi"}),
            description: None,
            tool_use_id: None,
            session_id: None,
        };
        let q = format_approval_question(&req);
        assert!(q.contains("Bash"));
        assert!(q.contains("echo hi"));
        assert!(q.contains("允许"));
    }

    /// A fake agent that emits a permission_request, waits for the bridge's
    /// permission_response on stdin, then emits a final result event.
    fn permission_ask_script() -> &'static str {
        r#"read -r _turn; \
printf '%s\n' '{"type":"permission_request","request_id":"r1","tool_name":"Bash","input":{"command":"rm -rf x"}}'; \
read -r _resp; \
printf '%s\n' '{"type":"result","text":"after-permission"}'"#
    }

    fn ask_profile_yaml(script: &str, ask_timeout: u64) -> String {
        format!(
            "{}permission: true\npermission_default: ask\npermission_ask_timeout_secs: {}\nstreaming: false\n",
            shell_profile_yaml(script, 30),
            ask_timeout
        )
    }

    fn reply_msg(text: &str) -> WeixinMessage {
        use crate::ilink::types::{MessageItem, TextItem};
        WeixinMessage {
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some(text.to_string()),
                }),
                ..Default::default()
            }])),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ask_permission_allows_on_user_yes() {
        let yaml = ask_profile_yaml(permission_ask_script(), 10);
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hi").unwrap();

        let broker = ApprovalBroker::new();
        let deliver_broker = broker.clone();
        tokio::spawn(async move {
            // Give run_cli a moment to register its inbox and send the prompt.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                deliver_broker.deliver("test", &reply_msg("允许")),
                "inbox should be registered by now"
            );
        });

        let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);
        let chunks = spawn_partial_collector(partial_rx);
        let (body, _sid, summary) = run_cli(
            profile,
            "ask_profile",
            "hi",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            broker,
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        assert_eq!(body, "after-permission");
        assert!(!summary.error_event);
        let chunks = chunks.await.expect("collector panicked");
        assert!(
            chunks.iter().any(|c| c.contains("已允许")),
            "expected an allowance confirmation partial, got {chunks:?}"
        );
    }

    #[tokio::test]
    async fn ask_permission_denies_on_timeout() {
        // 1s timeout; never deliver a reply.
        let yaml = ask_profile_yaml(permission_ask_script(), 1);
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hi").unwrap();

        let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);
        let chunks = spawn_partial_collector(partial_rx);
        let (body, _sid, _summary) = run_cli(
            profile,
            "ask_profile",
            "hi",
            "",
            "default",
            "user-1",
            "ctx-1",
            &[],
            partial_tx,
            ApprovalBroker::new(),
            "test".to_string(),
        )
        .await
        .expect("run_cli failed");

        // The agent receives a deny response and proceeds to emit its text.
        assert_eq!(body, "after-permission");
        let chunks = chunks.await.expect("collector panicked");
        assert!(
            chunks.iter().any(|c| c.contains("超时")),
            "expected a timeout notice partial, got {chunks:?}"
        );
    }

    /// Spawn a task that drains a partial watch channel into a Vec<String>,
    /// returning a handle to the collected chunks. Skips the initial `None`
    /// slot. Completes when the sender is dropped.
    fn spawn_partial_collector(
        mut rx: watch::Receiver<Option<String>>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut out = Vec::new();
            while rx.changed().await.is_ok() {
                if let Some(c) = rx.borrow_and_update().clone() {
                    out.push(c);
                }
            }
            out
        })
    }
}
