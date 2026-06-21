use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

use crate::bridge::config::{BridgeProfile, StdinMode};
use crate::ilink::types::WeixinMessage;
use crate::paths::expand_user_path;

/// Hard upper bound on how many bytes of a child's stdout/stderr we buffer in
/// memory before truncating. A misbehaving or malicious CLI could otherwise
/// stream unbounded output and OOM the Hub. This is purely a safety valve: the
/// final reply is separately truncated to `max_reply_chars` (default 8000), so
/// this cap is ~8000× any legitimate reply and never triggers in normal use.
pub const MAX_CLI_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// Replace `{{MESSAGE}}`, `{{SESSION_ID}}`, and `{{SESSION_NAME}}` in a template string.
///
/// SEC-003: `message` is user-controlled (forwarded WeChat message text). We
/// refuse to inject any string that contains bytes which would be interpreted
/// by a shell-style wrapper (`bash -c`, `sh -c`, `env` parsing) — NUL,
/// newlines, or carriage returns. Callers that route `message` into argv or
/// env vars should reject when this returns an error rather than silently
/// passing through, to prevent a user message from breaking out of its
/// intended slot.
pub(super) fn apply_placeholders(
    template: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
) -> Result<String, PlaceholderError> {
    validate_safe_value("message", message)?;
    validate_safe_value("session_id", session_id)?;
    validate_safe_value("session_name", session_name)?;
    Ok(template
        .replace("{{MESSAGE}}", message)
        .replace("{{SESSION_ID}}", session_id)
        .replace("{{SESSION_NAME}}", session_name))
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

#[derive(Debug, thiserror::Error)]
pub enum PlaceholderError {
    #[error("placeholder value for `{field}` contains NUL/newline; refusing to inject")]
    UnsafeValue { field: String },
}

/// If the first line of `stdout` starts with `prefix`, the remainder of that line is the CLI session id
/// (returned as `Some`); the rest of `stdout` (following lines) is the reply body. If `prefix` is empty
/// or the first line does not match, returns `(stdout, None)`.
pub(super) fn split_cli_session_from_stdout(
    prefix: &str,
    stdout: &str,
) -> (String, Option<String>) {
    if prefix.is_empty() {
        return (stdout.to_string(), None);
    }
    let mut lines = stdout.lines();
    let Some(first) = lines.next() else {
        return (stdout.to_string(), None);
    };
    if let Some(rest) = first.strip_prefix(prefix) {
        let sid = rest.trim();
        if sid.is_empty() {
            return (stdout.to_string(), None);
        }
        let rest_lines: String = lines.collect::<Vec<_>>().join("\n");
        return (rest_lines, Some(sid.to_string()));
    }
    (stdout.to_string(), None)
}

pub(super) fn truncate_chars(s: &str, max_chars: usize, suffix: &str) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let budget = max_chars.saturating_sub(suffix.chars().count());
    s.chars().take(budget).collect::<String>() + suffix
}

/// Extract media-related environment variables from a WeChat message so that CLI scripts
/// can handle image / file / video inputs without manually parsing the full JSON payload.
pub(super) fn extract_media_env(msg: &WeixinMessage) -> Vec<(String, String)> {
    use crate::ilink::types::msg_type;
    let mut env = Vec::new();
    let items = match msg.item_list.as_ref() {
        Some(l) => l,
        None => return env,
    };
    for item in items.iter() {
        match item.item_type {
            Some(msg_type::IMAGE) => {
                env.push(("ILINK_ITEM_TYPE".into(), "image".into()));
                if let Some(url) = item.image_item.as_ref().and_then(|i| i.cdn_url.as_deref()) {
                    if !url.is_empty() {
                        env.push(("ILINK_IMAGE_URL".into(), url.to_string()));
                    }
                }
                break;
            }
            Some(msg_type::FILE) => {
                env.push(("ILINK_ITEM_TYPE".into(), "file".into()));
                if let Some(fi) = item.file_item.as_ref() {
                    if let Some(url) = fi.cdn_url.as_deref().filter(|s| !s.is_empty()) {
                        env.push(("ILINK_FILE_URL".into(), url.to_string()));
                    }
                    if let Some(name) = fi.file_name.as_deref().filter(|s| !s.is_empty()) {
                        env.push(("ILINK_FILE_NAME".into(), name.to_string()));
                    }
                }
                break;
            }
            Some(msg_type::VIDEO) => {
                env.push(("ILINK_ITEM_TYPE".into(), "video".into()));
                if let Some(url) = item.video_item.as_ref().and_then(|v| v.cdn_url.as_deref()) {
                    if !url.is_empty() {
                        env.push(("ILINK_VIDEO_URL".into(), url.to_string()));
                    }
                }
                break;
            }
            _ => {}
        }
    }
    env
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
    media_env: &[(String, String)],
    partial_tx: mpsc::UnboundedSender<String>,
) -> Result<(String, Option<String>)> {
    let args: Vec<String> = cfg
        .args
        .iter()
        .map(|a| {
            apply_placeholders(a, message, session_id, session_name)
                .with_context(|| format!("unsafe placeholder value in args template `{a}`"))
        })
        .collect::<Result<_>>()?;

    let command = super::paths::resolve_spawn_command(&cfg.command);

    let mut cmd = Command::new(&command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &cfg.cwd {
        let dir = expand_user_path(
            &apply_placeholders(dir, message, session_id, session_name)
                .with_context(|| format!("unsafe placeholder value in cwd template `{dir}`"))?,
        );
        cmd.current_dir(&dir);
    }

    cmd.env("ILINK_MESSAGE", message);
    cmd.env("ILINK_SESSION_ID", session_id);
    cmd.env("ILINK_SESSION_NAME", session_name);
    cmd.env("ILINK_FROM_USER", from_user);
    cmd.env("ILINK_CONTEXT_TOKEN", context_token);
    cmd.env("ILINK_STREAMING", if cfg.streaming { "1" } else { "0" });

    for (k, v) in media_env {
        cmd.env(k, v);
    }

    for (k, v) in &cfg.env {
        let v = apply_placeholders(v, message, session_id, session_name)
            .with_context(|| format!("unsafe placeholder value in env var `{k}`"))?;
        let v = crate::bridge::config::expand_env_var_named(
            &v,
            &std::env::vars().collect(),
            Some(profile_name),
            Some(&format!("env.{k}")),
        )
        .with_context(|| format!("expand env var `{k}` for profile `{profile_name}`"))?;
        cmd.env(k, v);
    }

    match cfg.stdin {
        StdinMode::None => {
            cmd.stdin(std::process::Stdio::null());
        }
        StdinMode::Message => {
            cmd.stdin(std::process::Stdio::piped());
        }
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{command}`"))?;

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

    let stdin_task: Option<tokio::task::JoinHandle<Result<()>>> =
        if matches!(cfg.stdin, StdinMode::Message) {
            let mut stdin = child
                .stdin
                .take()
                .context("stdin pipe missing for stdin: message")?;
            let message_owned = message.to_string();
            Some(tokio::spawn(async move {
                stdin
                    .write_all(message_owned.as_bytes())
                    .await
                    .context("write stdin")?;
                stdin.shutdown().await.context("shutdown stdin")?;
                Ok(())
            }))
        } else {
            None
        };

    let streaming = cfg.streaming;
    let stream_result: Result<Vec<String>> =
        tokio::time::timeout(dur, async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(child_stdout);
            let mut final_lines: Vec<String> = Vec::new();
            let mut accumulated_bytes: usize = 0;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.context("read stdout")?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if let Some(json_part) = trimmed.strip_prefix("ILINK_PARTIAL:") {
                    if streaming {
                        match serde_json::from_str::<String>(json_part) {
                            Ok(chunk) => {
                                let _ = partial_tx.send(chunk);
                            }
                            Err(e) => {
                                warn!(error = %e, raw = %json_part, "failed to decode ILINK_PARTIAL chunk; skipping");
                            }
                        }
                    }
                    continue;
                }
                if accumulated_bytes >= MAX_CLI_CAPTURE_BYTES {
                    // Drop further reads entirely; previously-captured buffer
                    // is already at the cap so we must not grow it.
                    continue;
                }
                let projected = accumulated_bytes.saturating_add(line.len());
                if projected > MAX_CLI_CAPTURE_BYTES {
                    // Trim the line so the *total* buffer stays at the cap.
                    let remaining = MAX_CLI_CAPTURE_BYTES - accumulated_bytes;
                    line.truncate(remaining);
                    final_lines.push(line.clone());
                    accumulated_bytes = MAX_CLI_CAPTURE_BYTES;
                    warn!(
                        limit_bytes = MAX_CLI_CAPTURE_BYTES,
                        "CLI stdout exceeded capture limit; hard-truncating accumulated reply"
                    );
                } else {
                    accumulated_bytes = projected;
                    final_lines.push(line.clone());
                }
            }
            drop(partial_tx);
            Ok(final_lines)
        })
        .await
        .map_err(|_| anyhow::anyhow!("CLI timed out after {}s", cfg.timeout_secs))?;

    let final_lines = stream_result?;

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| anyhow::anyhow!("CLI failed to exit after stdout EOF"))?
        .context("wait for CLI process")?;

    if let Some(task) = stdin_task {
        match task.await {
            Ok(Err(e)) => warn!(error = %e, "stdin write error (non-fatal)"),
            Err(e) => warn!(error = %e, "stdin task panicked"),
            Ok(Ok(())) => {}
        }
    }

    let stderr = stderr_task.await.unwrap_or_default();
    if !stderr.is_empty() {
        tracing::debug!(stderr = %stderr, "CLI stderr");
    }

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let stdout_str: String = final_lines.concat();
        anyhow::bail!(
            "command exited with status {code}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout_str}"
        );
    }

    let mut stdout = final_lines.concat();

    if cfg.include_stderr_in_reply && !stderr.is_empty() {
        stdout.push_str("\n--- stderr ---\n");
        stdout.push_str(&stderr);
    }

    let prefix = cfg
        .cli_session_first_line_prefix
        .as_deref()
        .unwrap_or("")
        .trim();
    let (body, cli_sid) = split_cli_session_from_stdout(prefix, &stdout);
    Ok((body, cli_sid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::config::BridgeApp;
    use tokio::sync::mpsc;

    #[test]
    fn placeholders_message_session_id_and_name() {
        assert_eq!(
            apply_placeholders(
                "{{MESSAGE}}|{{SESSION_ID}}|{{SESSION_NAME}}",
                "hi",
                "sid-9",
                "feat-a"
            )
            .unwrap(),
            "hi|sid-9|feat-a"
        );
    }

    #[test]
    fn placeholders_reject_nul_in_message() {
        let err = apply_placeholders("{{MESSAGE}}", "evil\0payload", "sid", "name").unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    #[test]
    fn placeholders_reject_newline_in_session_id() {
        // A newline in SESSION_ID could break out of a quoted arg slot.
        let err = apply_placeholders("session={{SESSION_ID}}", "msg", "sid\nrm -rf /", "name")
            .unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    #[test]
    fn placeholders_reject_carriage_return_in_session_name() {
        let err =
            apply_placeholders("name={{SESSION_NAME}}", "msg", "sid", "feat\r\noops").unwrap_err();
        assert!(matches!(err, PlaceholderError::UnsafeValue { .. }));
    }

    #[test]
    fn placeholder_session_name_defaults_to_default() {
        assert_eq!(
            apply_placeholders("name={{SESSION_NAME}}", "", "", "default").unwrap(),
            "name=default"
        );
    }

    #[test]
    fn split_cli_session_first_line() {
        let (body, sid) =
            split_cli_session_from_stdout("ILINK_SESSION:", "ILINK_SESSION:uuid-1\nhello\n");
        assert_eq!(sid.as_deref(), Some("uuid-1"));
        assert_eq!(body, "hello");
    }

    #[test]
    fn split_cli_session_no_match_returns_full() {
        let (body, sid) = split_cli_session_from_stdout("ILINK_SESSION:", "plain\n");
        assert!(sid.is_none());
        assert_eq!(body, "plain\n");
    }

    #[test]
    fn truncate_respects_chars() {
        let s = truncate_chars("abcde", 4, "…");
        assert_eq!(s, "abc…");
    }

    #[tokio::test]
    async fn test_stdin_write_timeout() {
        let sleep_cmd = if cfg!(target_os = "macos") {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let yaml =
            format!("command: {sleep_cmd}\nargs: [\"10\"]\nstdin: message\ntimeout_secs: 1\n");
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        let large_msg = "A".repeat(128 * 1024);

        let start = std::time::Instant::now();
        let (partial_tx, _partial_rx) = mpsc::unbounded_channel::<String>();
        let res = run_cli(
            profile,
            "test_profile",
            &large_msg,
            "session-123",
            "session-name",
            "user-123",
            "ctx-123",
            &[],
            partial_tx,
        )
        .await;

        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "Expected stdin write to timeout, but it succeeded: {:?}",
            res
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("timed out")
                || err_msg.contains("stdin")
                || err_msg.contains("spawn")
                || err_msg.contains("No such file"),
            "Expected timeout or spawn error, got: {}",
            err_msg
        );
        assert!(elapsed.as_secs() < 3, "Took too long: {:?}", elapsed);
    }
}
