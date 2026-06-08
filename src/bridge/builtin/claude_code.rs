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
use tokio::io::AsyncWriteExt;
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

async fn call_claude(
    message: &str,
    session_id: &str,
) -> Result<(String, Option<String>)> {
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "json".into(),
        "--dangerously-skip-permissions".into(),
    ];

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    let mut cmd = Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn `claude`; ensure it is installed and in PATH")?;

    if !message.is_empty() {
        let mut stdin = child.stdin.take().context("claude stdin pipe missing")?;
        stdin.write_all(message.as_bytes()).await.context("write to claude stdin")?;
        stdin.shutdown().await.context("shutdown claude stdin")?;
    }

    let output = child.wait_with_output().await.context("wait for claude")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "claude exited with status {:?}\nstderr: {stderr}",
            output.status.code()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_claude_json_output(&stdout)
}

/// Parse `claude --output-format json` output.
///
/// Claude prints one JSON object. We look for the object with `type == "result"`.
/// Falls back to the last non-empty line if no typed result is found.
fn parse_claude_json_output(stdout: &str) -> Result<(String, Option<String>)> {
    // Try to find a result object (may be last line in stream-json, or the whole output in json mode)
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<ClaudeJsonResult>(line) {
            let is_result = obj.msg_type.as_deref() == Some("result")
                || obj.subtype.as_deref() == Some("success");
            if is_result || obj.result.is_some() {
                let text = obj.result.unwrap_or_default();
                return Ok((text, obj.session_id));
            }
        }
    }

    // Fallback: treat all stdout as the reply (no session ID extraction)
    Ok((stdout.to_string(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_object() {
        let json = r#"{"type":"result","subtype":"success","result":"Hello!","session_id":"sess-abc"}"#;
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
}
