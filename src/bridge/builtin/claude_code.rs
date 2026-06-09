//! Built-in `claude-code` profile: wraps the `claude` CLI with session continuity.
//!
//! Reads P0 env vars, calls `claude --output-format json [--resume <uuid>]`,
//! extracts the new session_id from the JSON response, then writes:
//!
//!   ILINK_SESSION:<new_session_id>
//!   <response text>
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;

/// JSON shape of `claude --output-format json` final result line.
/// Claude outputs one JSON object per line (stream-json) or a single object (json).
/// We only need the last complete object with a `result` field.
#[derive(Debug, Deserialize)]
struct ClaudeJsonResult {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    result: Option<String>,
    session_id: Option<String>,
    // present in stream-json format
    subtype: Option<String>,
}

pub async fn run() -> Result<()> {
    let message = std::env::var("ILINK_MESSAGE").unwrap_or_default();
    let session_id = std::env::var("ILINK_SESSION_ID").unwrap_or_default();

    let (response_text, new_session_id) = if !session_id.is_empty() {
        match call_claude(&message, &session_id).await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("[claude-code] --resume failed ({e:#}), retrying as new session");
                call_claude(&message, "").await?
            }
        }
    } else {
        call_claude(&message, "").await?
    };

    // P0 output format: optional session line first, then response text
    if let Some(sid) = &new_session_id {
        if !sid.is_empty() {
            println!("ILINK_SESSION:{sid}");
        }
    }
    print!("{response_text}");

    Ok(())
}

async fn call_claude(message: &str, session_id: &str) -> Result<(String, Option<String>)> {
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "json".into(),
        "--dangerously-skip-permissions".into(),
    ];

    // Allow overriding the model via env var (useful when claude's default is misconfigured).
    if let Ok(model) = std::env::var("ILINK_CLAUDE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    // Use -p flag for non-interactive mode (clean JSON output, no stdin ambiguity).
    args.push("-p".into());
    args.push(message.to_string());

    let mut cmd = Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd
        .spawn()
        .context("failed to spawn `claude`; ensure it is installed and in PATH")?;
    let output = child.wait_with_output().await.context("wait for claude")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Claude may exit non-zero yet still emit parseable JSON with a user-facing `result`
    // (e.g. model_not_found). Prefer that over a bare exit-code error.
    if let Ok((text, sid)) = parse_claude_json_output(&stdout) {
        if !text.trim().is_empty() {
            if !output.status.success() {
                eprintln!(
                    "[claude-code] claude exited {:?} but returned result text",
                    output.status.code()
                );
            }
            return Ok((text, sid));
        }
    }

    if !output.status.success() {
        let mut detail = stderr.to_string();
        if detail.trim().is_empty() && !stdout.trim().is_empty() {
            detail = stdout.to_string();
        }
        anyhow::bail!(
            "claude exited with status {:?}\nstderr: {detail}",
            output.status.code()
        );
    }

    parse_claude_json_output(&stdout)
}

/// Parse `claude --output-format json` output.
///
/// Claude may output either:
/// - A JSON array `[{...}, {...}]` on a single line (non-interactive mode with `-p`)
/// - Newline-delimited JSON objects (stream-json format)
///
/// We look for the object with `type == "result"` to extract the response text and session ID.
fn parse_claude_json_output(stdout: &str) -> Result<(String, Option<String>)> {
    let items = collect_json_items(stdout);

    // Search for the result object
    for obj in items.into_iter().rev() {
        let is_result =
            obj.msg_type.as_deref() == Some("result") || obj.subtype.as_deref() == Some("success");
        if is_result || obj.result.is_some() {
            let text = obj.result.unwrap_or_default();
            return Ok((text, obj.session_id));
        }
    }

    // Fallback: treat all stdout as the reply (no session ID extraction)
    Ok((stdout.to_string(), None))
}

/// Collect JSON items from claude output, handling both array and newline-delimited formats.
fn collect_json_items(stdout: &str) -> Vec<ClaudeJsonResult> {
    let trimmed = stdout.trim();

    // Try JSON array format first (claude -p produces this)
    if trimmed.starts_with('[') {
        if let Ok(items) = serde_json::from_str::<Vec<ClaudeJsonResult>>(trimmed) {
            return items;
        }
    }

    // Fall back to newline-delimited JSON objects
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<ClaudeJsonResult>(line).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_object() {
        let json =
            r#"{"type":"result","subtype":"success","result":"Hello!","session_id":"sess-abc"}"#;
        let (text, sid) = parse_claude_json_output(json).unwrap();
        assert_eq!(text, "Hello!");
        assert_eq!(sid.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn parse_multiline_stream_last_result() {
        let stdout = r#"{"type":"assistant","content":"thinking..."}
{"type":"result","subtype":"success","result":"Done.","session_id":"sess-xyz"}
"#;
        let (text, sid) = parse_claude_json_output(stdout).unwrap();
        assert_eq!(text, "Done.");
        assert_eq!(sid.as_deref(), Some("sess-xyz"));
    }

    #[test]
    fn fallback_to_raw_stdout_when_no_json() {
        let stdout = "plain text response\n";
        let (text, sid) = parse_claude_json_output(stdout).unwrap();
        assert_eq!(text, "plain text response\n");
        assert!(sid.is_none());
    }

    #[test]
    fn parse_error_result_from_json_array() {
        let stdout = r#"[{"type":"result","subtype":"success","is_error":true,"result":"model not found","session_id":"sess-1"}]"#;
        let (text, sid) = parse_claude_json_output(stdout).unwrap();
        assert_eq!(text, "model not found");
        assert_eq!(sid.as_deref(), Some("sess-1"));
    }
}
