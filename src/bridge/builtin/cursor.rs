//! Built-in `cursor` profile: wraps the Cursor `agent` CLI with session continuity.
//!
//! Reads P0 env vars, calls `agent --print --trust --yolo --output-format stream-json
//! [--model <model>] [--resume <uuid>]`, and streams text output to the parent bridge
//! via `ILINK_PARTIAL:` stdout lines.
//!
//! Message is written to the `agent` process stdin (unlike `claude` which uses `-p`).
//!
//! Each assistant text chunk is written immediately as:
//!
//!   ILINK_PARTIAL:<json-encoded-string>
//!
//! When the stream ends, the final P0 session line is written:
//!
//!   ILINK_SESSION:<new_session_id>
//!
//! The response body is left empty so the bridge does not send a duplicate final message.
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::common;

/// The Cursor agent uses the same `stream-json` schema as the Claude CLI, so the
/// shared [`common::StreamJsonEvent`] type parses its event lines too.
type CursorStreamEvent = common::StreamJsonEvent;

pub async fn run() -> Result<()> {
    let (message, session_id) = common::read_message_and_session();

    let new_session_id =
        common::with_session_resume_fallback("cursor", &message, &session_id, |m, s| async move {
            stream_cursor(&m, &s).await
        })
        .await?;

    // P0 output: optional session line only.
    // All response text was already streamed via ILINK_PARTIAL during execution.
    common::emit_session_line(new_session_id.as_deref());

    Ok(())
}

/// Call `agent --output-format stream-json`, emit every assistant text chunk as an
/// `ILINK_PARTIAL:` stdout line, and return the session ID from the result event.
///
/// All visible response text is streamed via ILINK_PARTIAL. When the model uses tools
/// between turns, the final assistant reply may only appear in `result.result` (with no
/// preceding `assistant` event); we emit it as an extra ILINK_PARTIAL in that case.
async fn stream_cursor(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut args: Vec<String> = vec![
        "--print".into(),
        "--trust".into(),
        "--yolo".into(),
        "--output-format".into(),
        "stream-json".into(),
    ];

    if let Ok(model) = std::env::var("CURSOR_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    let agent_exe = crate::bridge::paths::find_tool_with_extra_paths("agent");
    let mut cmd = Command::new(&agent_exe);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `agent`; ensure Cursor Agent CLI is installed and in PATH")?;

    // Write the message to agent's stdin, then close it.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(message.as_bytes())
            .await
            .context("write message to agent stdin")?;
        // stdin is dropped here, closing the pipe
    }

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = common::spawn_capped_drain(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut assistant_event_count: u32 = 0;
    let mut assistant_total_chars: usize = 0;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read agent stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<CursorStreamEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    let text = msg.text();
                    if !text.is_empty() {
                        assistant_event_count += 1;
                        assistant_total_chars += text.len();
                        eprintln!(
                            "[cursor] assistant#{} len={} total_so_far={}",
                            assistant_event_count,
                            text.len(),
                            assistant_total_chars
                        );
                        common::emit_partial(&text)?;
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id;
                // Cursor agent's result.result = concat of all assistant texts.
                // Since all assistant events are already streamed via ILINK_PARTIAL,
                // sending result would duplicate the full content to the user.
                // Disabled: only log for observability.
                if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    eprintln!(
                        "[cursor] result len={} assistant_events={} assistant_chars={} (NOT sending)",
                        result_text.len(),
                        assistant_event_count,
                        assistant_total_chars,
                    );
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for agent")?;
    let stderr = stderr_task.await.unwrap_or_default();

    common::ensure_success("agent", status, &stderr, found_session_id.is_some())?;

    Ok(found_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_result_event() {
        let json = r#"{"type":"result","result":"Hello!","session_id":"sess-abc"}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("result"));
        assert_eq!(event.result.as_deref(), Some("Hello!"));
        assert_eq!(event.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn deserialize_assistant_event_with_text_block() {
        let json =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi there"}]}}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("assistant"));
        let blocks = event.message.unwrap().content.unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type.as_deref(), Some("text"));
        assert_eq!(blocks[0].text.as_deref(), Some("Hi there"));
    }

    #[test]
    fn deserialize_unknown_event_does_not_panic() {
        let json = r#"{"type":"system","subtype":"init","session_id":"sess-new"}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("system"));
        assert!(event.message.is_none());
    }
}
