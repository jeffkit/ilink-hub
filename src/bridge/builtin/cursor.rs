//! Built-in `cursor` profile: wraps the Cursor `agent` CLI with session continuity.
//!
//! Reads P0 env vars, calls `agent --print --trust --yolo --output-format stream-json
//! [--model <model>] [--resume <uuid>]`, and streams text output to the parent bridge
//! via `AGENT_PARTIAL:` stdout lines.
//!
//! Message is written to the `agent` process stdin (unlike `claude` which uses `-p`).
//!
//! Each assistant text chunk is written immediately as:
//!
//!   AGENT_PARTIAL:<json-encoded-string>
//!
//! When the stream ends, the final P0 session line is written:
//!
//!   AGENT_SESSION:<new_session_id>
//!
//! The response body is left empty so the bridge does not send a duplicate final message.
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::debug;

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
    // All response text was already streamed via AGENT_PARTIAL during execution.
    common::emit_session_line(new_session_id.as_deref());

    Ok(())
}

/// Call `agent --output-format stream-json`, emit every assistant text chunk as an
/// `AGENT_PARTIAL:` stdout line, and return the session ID from the result event.
///
/// All visible response text is streamed via AGENT_PARTIAL. Cursor's `result.result`
/// is the concatenation of every assistant text already streamed, so it is not re-sent
/// in the normal case. The sole exception: when **no** assistant text events were emitted
/// (the model responded with tool-only actions), `result.result` is used as a fallback
/// so the user receives at least one message.
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
                    // Guard with trim() so that whitespace-only text blocks (e.g. a bare
                    // "\n" between tool calls) do not produce an empty-looking AGENT_PARTIAL.
                    if !text.trim().is_empty() {
                        assistant_event_count += 1;
                        assistant_total_chars += text.len();
                        debug!(
                            event = assistant_event_count,
                            len = text.len(),
                            total_chars = assistant_total_chars,
                            "cursor assistant chunk"
                        );
                        common::emit_partial(&text)?;
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id;
                if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    if assistant_event_count == 0 {
                        // No assistant text events were emitted: the model responded with
                        // tool-only actions and produced no explanatory text during the run.
                        // result.result is our only source of content; emit it as a fallback
                        // so the user receives at least one message instead of total silence.
                        debug!(
                            len = result_text.len(),
                            "cursor result fallback (0 assistant events)"
                        );
                        common::emit_partial(&result_text)?;
                    } else {
                        // Normal case: Cursor's result.result = concat of all assistant texts
                        // already streamed. Re-sending would duplicate the full conversation.
                        debug!(
                            len = result_text.len(),
                            assistant_events = assistant_event_count,
                            assistant_chars = assistant_total_chars,
                            "cursor result skipped (already streamed)"
                        );
                    }
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

    // ── Bug-fix regression tests ─────────────────────────────────────────────

    /// Bug #2: whitespace-only text blocks must not be counted as assistant events
    /// or emitted as AGENT_PARTIAL. The old guard `!text.is_empty()` passed "\n";
    /// the fix uses `!text.trim().is_empty()`.
    #[test]
    fn whitespace_only_text_is_not_an_assistant_event() {
        for ws in ["\n", "  ", "\t", "\r\n"] {
            let json = format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{ws}"}}]}}}}"#,
                ws = ws
                    .replace('\n', "\\n")
                    .replace('\t', "\\t")
                    .replace('\r', "\\r")
            );
            let event: CursorStreamEvent = serde_json::from_str(&json).unwrap();
            let text = event.message.unwrap().text();
            assert!(
                !text.is_empty(),
                "raw '{ws:?}' is non-empty — old guard would count it"
            );
            assert!(
                text.trim().is_empty(),
                "trimmed '{ws:?}' is empty → must not be counted as assistant event"
            );
        }
    }

    /// Real content with trailing newline must still pass the trim() guard.
    #[test]
    fn text_with_real_content_passes_guard() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Step 1 done.\n"}]}}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        let text = event.message.unwrap().text();
        assert!(!text.trim().is_empty(), "real content must not be blocked");
    }

    /// Bug #1 (Cursor): when `assistant_event_count == 0` (tool-only response) and
    /// `result.result` is non-empty, the bridge must emit result.result as a fallback.
    #[test]
    fn result_is_fallback_when_no_assistant_events() {
        let json = r#"{"type":"result","result":"Shell executed successfully.","session_id":"sf"}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        let result_text = event.result.as_deref().unwrap_or("");
        let assistant_event_count: u32 = 0;

        let should_fallback = assistant_event_count == 0 && !result_text.trim().is_empty();
        assert!(
            should_fallback,
            "when 0 assistant events and non-empty result, bridge must emit fallback"
        );
    }

    /// Normal case: when assistant events were already emitted, result.result
    /// (which equals their concat) must NOT be re-sent.
    #[test]
    fn result_is_not_resent_when_assistant_events_exist() {
        let json = r#"{"type":"result","result":"Step 1\nStep 2","session_id":"sc"}"#;
        let event: CursorStreamEvent = serde_json::from_str(json).unwrap();
        let result_text = event.result.as_deref().unwrap_or("");
        let assistant_event_count: u32 = 2; // 2 partials already streamed

        let should_fallback = assistant_event_count == 0 && !result_text.trim().is_empty();
        assert!(
            !should_fallback,
            "result must NOT be re-sent when assistant events already streamed"
        );
    }

    /// Edge case: tool-only run with empty result.result must remain completely
    /// silent — there is no content to deliver, and an empty partial would confuse
    /// message consumers.
    #[test]
    fn empty_result_with_no_assistant_events_stays_silent() {
        let result_text = "";
        let assistant_event_count: u32 = 0;
        let should_fallback = assistant_event_count == 0 && !result_text.trim().is_empty();
        assert!(
            !should_fallback,
            "empty result.result with no assistant events must stay silent"
        );
    }
}
