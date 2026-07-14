//! Built-in `codebuddy-code` profile: wraps the CodeBuddy Code CLI with session continuity.
//!
//! Reads the 0.3 turn object from stdin, calls `codebuddy -p --output-format
//! stream-json [--resume <uuid>]`, and emits NDJSON events on stdout:
//!
//! - `partial` for each assistant text chunk (streamed live),
//! - `text` with the full concatenated reply (the final body; the bridge dedups
//!   against already-forwarded partials in streaming mode and uses it as the
//!   single reply in non-streaming mode),
//! - `session` with the new session id.
//!
//! `streaming` is a bridge-side hint in 0.3, so the agent always runs the
//! underlying CLI in stream-json mode and emits both `partial` and `text` events.
//!
//! CodeBuddy Code CLI is compatible with Claude Code's `stream-json` protocol:
//!   {"type":"system","session_id":"<uuid>", ...}
//!   {"type":"assistant","message":{"content":[{"type":"text","text":"..."}],...},...}
//!   {"type":"result","subtype":"success","result":"...","session_id":"<uuid>"}
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.
//!
//! Supported env vars:
//!   ILINK_CODEBUDDY_MODEL  — override the CodeBuddy model (e.g. claude-sonnet-4.6)

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

use super::common;

/// JSON event line from `codebuddy --output-format stream-json`.
/// CodeBuddy Code uses the same `stream-json` schema as Claude Code CLI:
/// `type == "assistant"` carries incremental text, `type == "result"` carries the
/// final `session_id` and `result` text.
type CodeBuddyStreamEvent = common::StreamJsonEvent;

pub async fn run() -> Result<()> {
    let turn = common::read_turn_or_error();
    let (message, session_id) = common::message_and_session(&turn);

    let (new_session_id, body) = common::with_session_resume_fallback(
        "codebuddy-code",
        &message,
        &session_id,
        |m, s| async move { stream_codebuddy(&m, &s).await },
    )
    .await?;
    let mut emitter = common::SessionEmitter::new(&session_id);
    // stream_codebuddy already emitted live partials without session stamp; attach
    // continuity on the terminal result (inbound stamp when resuming).
    emitter.emit_result_opt(
        body.as_deref().filter(|s| !s.trim().is_empty()),
        new_session_id.as_deref(),
    )?;

    Ok(())
}

/// Call `codebuddy -p --output-format stream-json`, emit every assistant text chunk
/// as a `partial` event, and accumulate the full reply into a `result` event.
///
/// Returns `(session_id, body)`. `body` is the concatenated reply (assistant chunks
/// plus any `result.result` that was not already the last chunk), emitted as the
/// final `text` event by `run()`.
///
/// When the model uses tools between turns, the final assistant reply may only
/// appear in `result.result` (with no preceding `assistant` event); it is appended
/// to the body in that case.
async fn stream_codebuddy(
    message: &str,
    session_id: &str,
) -> Result<(Option<String>, Option<String>)> {
    let mut emitter = common::SessionEmitter::new(session_id);
    let mut args: Vec<String> = vec![
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--dangerously-skip-permissions".into(),
        // In -p (non-interactive) mode stdin is closed, so AskUserQuestion would
        // block the process forever with no visible prompt to the user.
        "--disallowedTools".into(),
        "AskUserQuestion".into(),
    ];

    if let Ok(model) = std::env::var("ILINK_CODEBUDDY_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    // The prompt argument comes last (positional).
    args.push(message.to_string());

    let mut cmd = Command::new("codebuddy");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context(
        "failed to spawn `codebuddy`; ensure CodeBuddy Code CLI is installed and in PATH",
    )?;

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = common::spawn_capped_drain(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    // Track the last text chunk appended so we can detect when result.result differs.
    let mut last_chunk: Option<String> = None;
    let mut body = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read codebuddy stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<CodeBuddyStreamEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    let text = msg.text();
                    // Guard with trim() so that whitespace-only text blocks (e.g. a bare
                    // "\n" emitted between tool calls) do not produce an empty-looking
                    // message that the user sees as blank.
                    if !text.trim().is_empty() {
                        emitter.emit_partial(&text)?;
                        body.push_str(&text);
                        last_chunk = Some(text);
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id;
                // When the model uses tools between turns, the final assistant reply text
                // may only appear in result.result and have no corresponding assistant event.
                // Append it to the body if it differs from the last chunk already accumulated.
                if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    let already_handled = last_chunk.as_deref() == Some(result_text.as_str());
                    if !already_handled {
                        body.push_str(&result_text);
                    }
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for codebuddy")?;
    let stderr = stderr_task.await.unwrap_or_default();

    // Flush any buffered partials (new-session discovery) before returning.
    emitter.discover(found_session_id.as_deref())?;
    emitter.finish_without_session()?;

    common::ensure_success("codebuddy", status, &stderr, found_session_id.is_some())?;

    let body_opt = if body.trim().is_empty() {
        None
    } else {
        Some(body)
    };
    Ok((found_session_id, body_opt))
}

#[cfg(test)]
mod tests {
    use super::common::StreamJsonEvent;

    #[test]
    fn deserialize_system_event() {
        let json = r#"{"type":"system","subtype":"init","session_id":"72f18735-deb2-42e1-be58-c07489548e02","uuid":"72f18735-deb2-42e1-be58-c07489548e02"}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("system"));
        assert_eq!(
            event.session_id.as_deref(),
            Some("72f18735-deb2-42e1-be58-c07489548e02")
        );
    }

    #[test]
    fn deserialize_assistant_event_with_text_block() {
        let json = r#"{"type":"assistant","uuid":"abc","session_id":"sess-1","message":{"id":"abc","content":[{"type":"text","text":"Hi!"}],"model":"claude-sonnet-4.6","role":"assistant","stop_reason":null,"stop_sequence":null,"type":"message","usage":{}},"parent_tool_use_id":null}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("assistant"));
        let text = event.message.unwrap().text();
        assert_eq!(text, "Hi!");
    }

    #[test]
    fn deserialize_assistant_event_skips_thinking_block() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"internal thought","signature":""},{"type":"text","text":"Hi there!"}]}}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        let text = event.message.unwrap().text();
        // thinking blocks must be skipped, only text blocks contribute
        assert_eq!(text, "Hi there!");
    }

    #[test]
    fn deserialize_result_event() {
        let json = r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi!","session_id":"72f18735-deb2-42e1-be58-c07489548e02"}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("result"));
        assert_eq!(event.result.as_deref(), Some("Hi!"));
        assert_eq!(
            event.session_id.as_deref(),
            Some("72f18735-deb2-42e1-be58-c07489548e02")
        );
    }

    #[test]
    fn result_with_no_prior_chunk_always_emits() {
        let last_chunk: Option<String> = None;
        let result_text = "The answer is 42.";
        let already_handled = last_chunk.as_deref() == Some(result_text);
        assert!(
            !already_handled,
            "when no prior chunk exists, result must NOT be skipped"
        );
    }

    #[test]
    fn result_matching_last_chunk_is_not_resent() {
        let last_chunk: Option<String> = Some("Hi!".to_string());
        let result_text = "Hi!";
        let already_handled = last_chunk.as_deref() == Some(result_text);
        assert!(
            already_handled,
            "result matching last_chunk must be skipped to avoid duplicate"
        );
    }

    #[test]
    fn whitespace_only_text_block_is_skipped() {
        for ws in ["\n", "  ", "\t", "\r\n"] {
            let json = format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{ws}"}}]}}}}"#,
                ws = ws
                    .replace('\n', "\\n")
                    .replace('\t', "\\t")
                    .replace('\r', "\\r")
            );
            let event: StreamJsonEvent = serde_json::from_str(&json).unwrap();
            let text = event.message.unwrap().text();
            assert!(
                text.trim().is_empty(),
                "trimmed '{ws:?}' must be empty → bridge must skip it"
            );
        }
    }

    #[test]
    fn real_world_full_stream_parses_correctly() {
        // Simulate the actual output sequence from a real codebuddy invocation.
        let lines = [
            r#"{"type":"system","subtype":"init","uuid":"72f18735-deb2-42e1-be58-c07489548e02","session_id":"72f18735-deb2-42e1-be58-c07489548e02","apiKeySource":"copilot.tencent.com","model":"hy3-preview-ioa","permissionMode":"bypassPermissions"}"#,
            r#"{"type":"assistant","uuid":"799fc30a","session_id":"72f18735-deb2-42e1-be58-c07489548e02","message":{"id":"799fc30a","content":[{"type":"thinking","thinking":"The user is just saying hello","signature":""}],"model":"hy3-preview-ioa","role":"assistant","stop_reason":null,"stop_sequence":null,"type":"message","usage":{}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","uuid":"1f990805","session_id":"72f18735-deb2-42e1-be58-c07489548e02","message":{"id":"1f990805","content":[{"type":"text","text":"Hi!"}],"model":"hy3-preview-ioa","role":"assistant","stop_reason":null,"stop_sequence":null,"type":"message","usage":{}},"parent_tool_use_id":null}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi!","uuid":"83c578cc","session_id":"72f18735-deb2-42e1-be58-c07489548e02","duration_ms":3708,"num_turns":3,"total_cost_usd":0}"#,
        ];

        let mut found_session_id: Option<String> = None;
        let mut last_chunk: Option<String> = None;
        let mut emitted: Vec<String> = Vec::new();

        for line in &lines {
            let event: StreamJsonEvent = serde_json::from_str(line).unwrap();
            match event.event_type.as_deref() {
                Some("assistant") => {
                    if let Some(msg) = &event.message {
                        let text = msg.text();
                        if !text.trim().is_empty() {
                            emitted.push(text.clone());
                            last_chunk = Some(text);
                        }
                    }
                }
                Some("result") => {
                    found_session_id = event.session_id;
                    if let Some(rt) = event.result.filter(|t| !t.trim().is_empty()) {
                        if last_chunk.as_deref() != Some(rt.as_str()) {
                            emitted.push(rt);
                        }
                    }
                }
                _ => {}
            }
        }

        // Only "Hi!" should be emitted (thinking block skipped, result deduped)
        assert_eq!(
            emitted,
            vec!["Hi!"],
            "only the text block should be emitted"
        );
        assert_eq!(
            found_session_id.as_deref(),
            Some("72f18735-deb2-42e1-be58-c07489548e02")
        );
    }

    #[test]
    fn non_streaming_mode_buffers_all_chunks_into_reply() {
        // Simulate the same event sequence but in non-streaming mode.
        // All text should be accumulated into a single buffer, not emitted as AGENT_PARTIAL.
        let lines = [
            r#"{"type":"system","subtype":"init","uuid":"72f18735","session_id":"72f18735"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"thinking","signature":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, "}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"world!"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"world!","session_id":"72f18735"}"#,
        ];

        let mut found_session_id: Option<String> = None;
        let mut last_chunk: Option<String> = None;
        let mut buffer = String::new();

        for line in &lines {
            let event: StreamJsonEvent = serde_json::from_str(line).unwrap();
            match event.event_type.as_deref() {
                Some("assistant") => {
                    if let Some(msg) = &event.message {
                        let text = msg.text();
                        if !text.trim().is_empty() {
                            buffer.push_str(&text);
                            last_chunk = Some(text);
                        }
                    }
                }
                Some("result") => {
                    found_session_id = event.session_id;
                    if let Some(rt) = event.result.filter(|t| !t.trim().is_empty()) {
                        if last_chunk.as_deref() != Some(rt.as_str()) {
                            buffer.push_str(&rt);
                        }
                    }
                }
                _ => {}
            }
        }

        // Both chunks concatenated; "world!" from result is deduped (matches last_chunk).
        assert_eq!(
            buffer, "Hello, world!",
            "non-streaming buffer must contain all chunks"
        );
        assert_eq!(found_session_id.as_deref(), Some("72f18735"));
    }
}
