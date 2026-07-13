//! Built-in `codex` profile: wraps the OpenAI Codex CLI with session continuity.
//!
//! Reads the 0.3 turn object from stdin, calls `codex exec [resume <thread_id>]
//! <message> --dangerously-bypass-approvals-and-sandbox --json`, and emits NDJSON
//! events on stdout:
//!
//! - `partial` for each `agent_message` item (streamed live),
//! - `text` with the concatenation of all `agent_message` items (the final body;
//!   the bridge dedups against already-forwarded partials in streaming mode),
//! - `session` with the thread id.
//!
//! `streaming` is a bridge-side hint in 0.3, so the agent always streams.
//!
//! JSONL event stream from `codex exec --json`:
//!   {"type":"thread.started","thread_id":"<uuid>"}
//!   {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
//!   {"type":"turn.completed","usage":{...}}
//!
//! If `exec resume` fails (thread expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

use super::common;

/// Top-level event shape from `codex exec --json`.
#[derive(Debug, Deserialize)]
struct CodexEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    /// Present on `type == "thread.started"`.
    thread_id: Option<String>,
    /// Present on `type == "item.completed"`.
    item: Option<CodexItem>,
}

/// Item payload within `item.completed` events.
#[derive(Debug, Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    /// Response text (present when `item_type == "agent_message"`).
    text: Option<String>,
}

pub async fn run() -> Result<()> {
    let turn = common::read_turn_or_error();
    let (message, session_id) = common::message_and_session(&turn);

    let new_thread_id =
        common::with_session_resume_fallback("codex", &message, &session_id, |m, s| async move {
            stream_codex(&m, &s).await
        })
        .await?;

    common::emit_session(new_thread_id.as_deref());

    Ok(())
}

/// Call `codex exec [resume <session_id>] <message> --json`, emit each `agent_message`
/// item as a `partial` event and the concatenation of all of them as a `text` event,
/// and return the thread ID.
async fn stream_codex(message: &str, session_id: &str) -> Result<Option<String>> {
    // Build: codex exec [resume <id>] <message> --dangerously-bypass-approvals-and-sandbox --json
    let mut args: Vec<String> = vec!["exec".into()];

    if !session_id.is_empty() {
        args.push("resume".into());
        args.push(session_id.to_string());
    }

    args.push(message.to_string());
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    args.push("--json".into());

    let mut cmd = Command::new("codex");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped()); // closed immediately below
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `codex`; ensure Codex CLI is installed and in PATH")?;

    // Close stdin immediately — codex reads the message from args, not stdin.
    drop(child.stdin.take());

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = common::spawn_capped_drain(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_thread_id: Option<String> = None;
    let mut final_text = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read codex stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<CodexEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("thread.started") => {
                found_thread_id = event.thread_id;
            }
            Some("item.completed") => {
                if let Some(item) = &event.item {
                    if item.item_type.as_deref() == Some("agent_message") {
                        if let Some(text) = &item.text {
                            if !text.trim().is_empty() {
                                common::emit_partial(text)?;
                                final_text.push_str(text);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for codex")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !final_text.trim().is_empty() {
        common::emit_text(&final_text)?;
    }

    common::ensure_success("codex", status, &stderr, found_thread_id.is_some())?;

    Ok(found_thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_thread_started() {
        let json =
            r#"{"type":"thread.started","thread_id":"019ecb0d-da0c-7002-bb29-a0fd8b2c2253"}"#;
        let event: CodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("thread.started"));
        assert_eq!(
            event.thread_id.as_deref(),
            Some("019ecb0d-da0c-7002-bb29-a0fd8b2c2253")
        );
    }

    #[test]
    fn deserialize_item_completed_agent_message() {
        let json = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hello"}}"#;
        let event: CodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("item.completed"));
        let item = event.item.unwrap();
        assert_eq!(item.item_type.as_deref(), Some("agent_message"));
        assert_eq!(item.text.as_deref(), Some("Hello"));
    }

    #[test]
    fn deserialize_turn_completed_does_not_panic() {
        let json = r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":20}}"#;
        let event: CodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("turn.completed"));
        assert!(event.item.is_none());
        assert!(event.thread_id.is_none());
    }
}
