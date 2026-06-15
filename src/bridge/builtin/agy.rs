//! Built-in `agy` profile: wraps the Antigravity (Google DeepMind) `agy` CLI with
//! session continuity.
//!
//! Unlike Claude Code or Cursor, `agy` outputs plain text to stdout (no stream-json).
//! The conversation ID is extracted from the agy log file written during execution.
//!
//! Session management:
//!   - New session: run `agy -p <message>`, parse `Created conversation <uuid>` from log
//!   - Resume: run `agy --conversation <uuid> -p <message>`, keep the same ID
//!
//! P0 output:
//!   ILINK_SESSION:<conversation_id>   (if available)
//!   <response text>
//!
//! Note: agy requires stdin to be a pipe (not a terminal); the handler closes stdin
//! immediately after spawning so agy does not block waiting for interactive input.
//!
//! If `--conversation` resume fails, automatically retries as a fresh session.

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn run() -> Result<()> {
    let message = std::env::var("ILINK_MESSAGE").unwrap_or_default();
    let session_id = std::env::var("ILINK_SESSION_ID").unwrap_or_default();

    let (response, new_session_id) = if !session_id.is_empty() {
        match call_agy(&message, &session_id).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[agy] --conversation resume failed ({e:#}), retrying as new session");
                call_agy(&message, "").await?
            }
        }
    } else {
        call_agy(&message, "").await?
    };

    // P0 output: optional session line first, then response text.
    if let Some(sid) = &new_session_id {
        if !sid.is_empty() {
            println!("ILINK_SESSION:{sid}");
        }
    }
    print!("{response}");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    Ok(())
}

/// Run `agy -p <message>` (optionally with `--conversation <id>`), capture stdout as
/// the response, and extract the conversation ID from the log file.
///
/// Returns `(response_text, Option<conversation_id>)`.
async fn call_agy(message: &str, session_id: &str) -> Result<(String, Option<String>)> {
    // Use a per-process temp log file to avoid conflicts when multiple profiles run.
    let log_path = format!("/tmp/agy-ilink-{}.log", std::process::id());

    let mut args: Vec<String> = vec![
        "--dangerously-skip-permissions".into(),
        "--log-file".into(),
        log_path.clone(),
    ];

    if let Ok(model) = std::env::var("AGY_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--conversation".into());
        args.push(session_id.to_string());
    }

    // `-p` / `--print` must be last (before the message argument).
    args.push("-p".into());
    args.push(message.to_string());

    let mut cmd = Command::new("agy");
    cmd.args(&args);
    // agy blocks if stdin is a terminal; close it immediately to run non-interactively.
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `agy`; ensure Antigravity CLI is installed and in PATH")?;

    // Drop stdin immediately so agy doesn't wait for interactive input.
    drop(child.stdin.take());

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, BufReader};
        let mut buf = Vec::new();
        BufReader::new(child_stderr).read_to_end(&mut buf).await.ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Collect full stdout (agy outputs plain text, no streaming events).
    let stdout_task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, BufReader};
        let mut buf = Vec::new();
        BufReader::new(child_stdout).read_to_end(&mut buf).await.ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    let status = child.wait().await.context("wait for agy")?;
    let stderr = stderr_task.await.unwrap_or_default();
    let stdout = stdout_task.await.unwrap_or_default();

    // Parse the conversation ID from the log file.
    let new_conv_id = if session_id.is_empty() {
        // New session: look for "Created conversation <uuid>".
        extract_conversation_id_from_log(&log_path, "Created conversation").await
    } else {
        // Resumed session: the same conversation ID persists.
        Some(session_id.to_string())
    };

    // Clean up temp log file (best-effort).
    let _ = tokio::fs::remove_file(&log_path).await;

    if !status.success() && stdout.trim().is_empty() {
        let detail = if !stderr.is_empty() { stderr } else { String::from("(no output)") };
        anyhow::bail!("agy exited with status {:?}\n{detail}", status.code());
    }

    Ok((stdout, new_conv_id))
}

/// Scan the agy log file for a line containing `prefix` followed by a UUID,
/// and return that UUID.
///
/// Example matching line:
///   `I0615 19:29:54.053019 92471 server.go:755] Created conversation 83b95686-35cf-4940-9857-f0ad892a346c`
async fn extract_conversation_id_from_log(log_path: &str, prefix: &str) -> Option<String> {
    let content = tokio::fs::read_to_string(log_path).await.ok()?;
    for line in content.lines() {
        let Some(pos) = line.find(prefix) else { continue };
        let after = line[pos + prefix.len()..].trim_start();
        // Take the first 36 characters and validate as UUID format.
        if after.len() >= 36 {
            let candidate = &after[..36];
            if is_uuid_like(candidate) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Quick UUID-format check: 8-4-4-4-12 hex digits separated by hyphens.
fn is_uuid_like(s: &str) -> bool {
    let parts: Vec<&str> = s.splitn(5, '-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected_lens.iter())
        .all(|(p, &len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_validation() {
        assert!(is_uuid_like("83b95686-35cf-4940-9857-f0ad892a346c"));
        assert!(is_uuid_like("00000000-0000-0000-0000-000000000000"));
        assert!(!is_uuid_like("not-a-uuid"));
        assert!(!is_uuid_like("83b95686-35cf-4940-9857"));
    }

    #[test]
    fn extract_from_log_line() {
        let line = "I0615 19:29:54.053019 92471 server.go:755] Created conversation 83b95686-35cf-4940-9857-f0ad892a346c";
        let prefix = "Created conversation";
        let pos = line.find(prefix).unwrap();
        let after = line[pos + prefix.len()..].trim_start();
        let candidate = &after[..36];
        assert!(is_uuid_like(candidate));
        assert_eq!(candidate, "83b95686-35cf-4940-9857-f0ad892a346c");
    }
}
