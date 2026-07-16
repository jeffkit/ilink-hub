//! Built-in `opencode` profile: wraps the OpenCode CLI with session continuity.
//!
//! Reads P0 env vars, calls:
//!   `opencode run --format json [--session <id>] [--model <m>] <message>`
//! and streams text output to the parent bridge via `AGENT_PARTIAL:` stdout lines.
//!
//! JSON event stream from `opencode run --format json` (JSONL, one object per line):
//!
//!   {"type":"step_start","sessionID":"ses_...","part":{...}}
//!   {"type":"tool_use","sessionID":"ses_...","part":{...}}
//!   {"type":"text","sessionID":"ses_...","part":{"text":"hello",...}}
//!   {"type":"step_finish","sessionID":"ses_...","part":{"reason":"stop",...}}
//!
//! Each `type == "text"` event's `part.text` is emitted immediately as:
//!
//!   AGENT_PARTIAL:<json-encoded-string>
//!
//! When the stream ends, the final P0 session line is written:
//!
//!   AGENT_SESSION:<sessionID>
//!
//! Model: set `ILINK_OPENCODE_MODEL` (e.g. `deepseek/deepseek-chat`). Optional;
//! if unset, OpenCode uses its default configured model.

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

use super::common;

/// Top-level event from `opencode run --format json`.
#[derive(Debug, Deserialize)]
struct OpenCodeEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    /// Session ID present on every event.
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    /// Payload; `text` field populated when `event_type == "text"`.
    part: Option<OpenCodePart>,
}

/// The `part` object. Only `text` is relevant; all other fields are ignored.
#[derive(Debug, Deserialize)]
struct OpenCodePart {
    text: Option<String>,
}

pub async fn run() -> Result<()> {
    let (message, session_id) = common::read_message_and_session();
    let is_streaming = std::env::var("AGENT_STREAMING").map_or(true, |v| v.trim() != "0");

    let new_session_id = common::with_session_resume_fallback(
        "opencode",
        &message,
        &session_id,
        |m, s| async move { stream_opencode(&m, &s, is_streaming).await },
    )
    .await?;

    // All response text was already streamed via AGENT_PARTIAL in streaming mode.
    // In non-streaming mode, stream_opencode handled the output directly and returned None.
    common::emit_session_line(new_session_id.as_deref());
    Ok(())
}

/// Call `opencode run --format json`, emit each text chunk as an `AGENT_PARTIAL:` stdout line
/// (streaming mode) or accumulate into a single reply block (non-streaming mode).
///
/// Returns `Some(session_id)` in streaming mode, `None` in non-streaming mode (output
/// was already written to stdout in the non-streaming path).
async fn stream_opencode(
    message: &str,
    session_id: &str,
    is_streaming: bool,
) -> Result<Option<String>> {
    // opencode run auto-approves all permissions in non-interactive mode;
    // no --dangerously-skip-permissions flag is needed (nor accepted).
    let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];

    if let Ok(model) = std::env::var("ILINK_OPENCODE_MODEL") {
        let model = model.trim().to_string();
        if !model.is_empty() {
            args.push("--model".into());
            args.push(model);
        }
    }

    if !session_id.is_empty() {
        args.push("--session".into());
        args.push(session_id.to_string());
    }

    args.push(message.to_string());

    let mut cmd = Command::new("opencode");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `opencode`; ensure it is installed and in PATH")?;

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = common::spawn_capped_drain(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut accumulated_text = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read opencode stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<OpenCodeEvent>(trimmed) else {
            continue;
        };

        // Capture the session ID from the first event that carries it.
        if found_session_id.is_none() {
            if let Some(sid) = event.session_id.filter(|s| !s.is_empty()) {
                found_session_id = Some(sid);
            }
        }

        if event.event_type.as_deref() == Some("text") {
            if let Some(part) = &event.part {
                if let Some(text) = &part.text {
                    // Guard with trim() so that whitespace-only text blocks do not
                    // produce an empty-looking AGENT_PARTIAL that the user sees as blank.
                    if !text.trim().is_empty() {
                        if is_streaming {
                            common::emit_partial(text)?;
                        } else {
                            accumulated_text.push_str(text);
                        }
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("wait for opencode")?;
    let stderr = stderr_task.await.unwrap_or_default();

    common::ensure_success("opencode", status, &stderr, found_session_id.is_some())?;

    // Non-streaming mode: emit session line first, then accumulated text, return None
    // so run() does not double-emit the session line.
    if !is_streaming {
        if !accumulated_text.is_empty() {
            if let Some(ref sid) = found_session_id {
                if !sid.is_empty() {
                    println!("AGENT_SESSION:{sid}");
                }
            }
            print!("{accumulated_text}");
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
        return Ok(None);
    }

    Ok(found_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text_event() {
        // Real-world shape from `opencode run --format json`.
        let json = r#"{
            "type": "text",
            "timestamp": 1783833255519,
            "sessionID": "ses_0ab40ac1bffe0ONHYkJhxblE5i",
            "part": {
                "id": "prt_abc",
                "sessionID": "ses_0ab40ac1bffe0ONHYkJhxblE5i",
                "messageID": "msg_xyz",
                "type": "text",
                "text": "hello",
                "time": {"start": 1783833255517, "end": 1783833255517}
            }
        }"#;
        let event: OpenCodeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("text"));
        assert_eq!(
            event.session_id.as_deref(),
            Some("ses_0ab40ac1bffe0ONHYkJhxblE5i")
        );
        let text = event.part.unwrap().text.unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn deserialize_step_start_captures_session_no_text() {
        let json = r#"{
            "type": "step_start",
            "timestamp": 1783833254579,
            "sessionID": "ses_0ab40ac1bffe0ONHYkJhxblE5i",
            "part": {
                "id": "prt_yyy",
                "sessionID": "ses_0ab40ac1bffe0ONHYkJhxblE5i",
                "messageID": "msg_zzz",
                "type": "step-start",
                "snapshot": "885b0d9ce46e"
            }
        }"#;
        let event: OpenCodeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("step_start"));
        assert_eq!(
            event.session_id.as_deref(),
            Some("ses_0ab40ac1bffe0ONHYkJhxblE5i")
        );
        // part.text is absent for non-text events.
        let text = event.part.and_then(|p| p.text);
        assert!(text.is_none());
    }

    #[test]
    fn deserialize_tool_use_does_not_panic() {
        let json = r#"{
            "type": "tool_use",
            "timestamp": 123,
            "sessionID": "ses_foo",
            "part": {"id": "prt_x", "tool": "bash", "state": {}}
        }"#;
        let event: OpenCodeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("tool_use"));
        assert_eq!(event.session_id.as_deref(), Some("ses_foo"));
    }

    #[test]
    fn deserialize_unknown_event_does_not_panic() {
        let json = r#"{"type": "step_finish", "sessionID": "ses_bar", "part": {}}"#;
        let event: OpenCodeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("step_finish"));
    }

    #[test]
    fn whitespace_only_text_is_skipped() {
        for ws in ["\n", "  ", "\t", "\r\n"] {
            assert!(
                ws.trim().is_empty(),
                "'{ws:?}' must be caught by trim() guard"
            );
        }
    }

    #[test]
    fn real_content_with_trailing_newline_passes_guard() {
        let text = "hello\n";
        assert!(
            !text.trim().is_empty(),
            "real content with trailing newline must not be blocked"
        );
    }

    #[test]
    fn session_id_captured_from_first_event() {
        // Session ID should be captured even from non-text events (step_start arrives first).
        let json = r#"{"type":"step_start","sessionID":"ses_captured","part":{}}"#;
        let event: OpenCodeEvent = serde_json::from_str(json).unwrap();
        let mut found: Option<String> = None;
        if found.is_none() {
            if let Some(sid) = event.session_id.filter(|s| !s.is_empty()) {
                found = Some(sid);
            }
        }
        assert_eq!(found.as_deref(), Some("ses_captured"));
    }
}
