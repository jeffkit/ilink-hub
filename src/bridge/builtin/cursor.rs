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
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;

/// JSON shape of a single event line from `agent --output-format stream-json`.
///
/// The Cursor agent uses the same stream-json schema as the Claude CLI:
/// `type == "assistant"` carries incremental text; `type == "result"` carries the
/// final session_id. The result text duplicates the last assistant event and is
/// therefore not separately output.
#[derive(Debug, Deserialize)]
struct CursorStreamEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    /// Session ID (present on `type == "result"` events).
    session_id: Option<String>,
    /// Present on `type == "assistant"` events.
    message: Option<CursorMessage>,
}

/// Nested message structure in `type == "assistant"` stream events.
#[derive(Debug, Deserialize)]
struct CursorMessage {
    content: Option<Vec<CursorContentBlock>>,
}

/// A single content block within a `CursorMessage`.
#[derive(Debug, Deserialize)]
struct CursorContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
}

pub async fn run() -> Result<()> {
    let message = std::env::var("ILINK_MESSAGE").unwrap_or_default();
    let session_id = std::env::var("ILINK_SESSION_ID").unwrap_or_default();

    let new_session_id = if !session_id.is_empty() {
        match stream_cursor(&message, &session_id).await {
            Ok(sid) => sid,
            Err(e) => {
                eprintln!("[cursor] --resume failed ({e:#}), retrying as new session");
                stream_cursor(&message, "").await?
            }
        }
    } else {
        stream_cursor(&message, "").await?
    };

    // P0 output: optional session line only.
    // All response text was already streamed via ILINK_PARTIAL during execution.
    if let Some(sid) = &new_session_id {
        if !sid.is_empty() {
            println!("ILINK_SESSION:{sid}");
        }
    }

    Ok(())
}

/// Call `agent --output-format stream-json`, emit every assistant text chunk as an
/// `ILINK_PARTIAL:` stdout line, and return the session ID from the result event.
///
/// All visible response text is streamed via ILINK_PARTIAL; the `result` field text is
/// not separately output because empirically it equals the last assistant text event,
/// which was already streamed.
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

    let mut cmd = Command::new("agent");
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
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        tokio::io::BufReader::new(child_stderr)
            .read_to_end(&mut buf)
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.context("read agent stdout")?;
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
                // Stream all assistant text blocks as ILINK_PARTIAL immediately.
                // Empirically, the `result` field equals only the last assistant event's
                // text, so every piece of text is captured here without duplication.
                if let Some(msg) = &event.message {
                    if let Some(blocks) = &msg.content {
                        let text: String = blocks
                            .iter()
                            .filter(|b| b.block_type.as_deref() == Some("text"))
                            .filter_map(|b| b.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            println!("ILINK_PARTIAL:{}", serde_json::to_string(&text)?);
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                        }
                    }
                }
            }
            Some("result") => {
                // Only extract session_id; the text is already covered by ILINK_PARTIAL above.
                found_session_id = event.session_id;
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for agent")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() && found_session_id.is_none() {
        let detail = if !stderr.is_empty() { stderr } else { String::from("(no output)") };
        anyhow::bail!("agent exited with status {:?}\nstderr: {detail}", status.code());
    }

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
        assert_eq!(event.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn deserialize_assistant_event_with_text_block() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi there"}]}}"#;
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
