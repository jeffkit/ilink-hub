//! Built-in `recursive` profile: wraps the `recursive` CLI with session continuity.
//!
//! AgentProc 0.4 agent: reads the NDJSON turn object from stdin, calls
//! `recursive --headless --output-format stream-json [-r <session_id>] -p <message>`,
//! and emits agentproc NDJSON events on stdout:
//!
//! - `{"type":"partial","text":...}` for each `assistant` text chunk (the bridge
//!   forwards these live when the profile's `streaming` hint is true).
//! - `{"type":"result","text":...,"session_id":...}` for the terminal body + continuity.
//! - `{"type":"error","message":...,"session_id":...}` when the terminal CLI
//!   `result` reports a failure.
//!
//! ## Output parsing
//!
//! `recursive --output-format stream-json` emits Claude Code–compatible NDJSON (same
//! schema as `claude --output-format stream-json`):
//!
//! ```json
//! {"type":"system","subtype":"init","session_id":"…"}
//! {"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}
//! {"type":"result","result":"hello","session_id":"…"}
//! ```
//!
//! ## Session continuity
//!
//! Session ID is taken from the terminal `result.session_id` when present, with a
//! fallback to the UUID extracted from stderr progress lines:
//!
//! ```text
//! session: recording to /path/to/sessions/<slug>/<uuid>/
//! ```
//!
//! Resume is requested with `-r <uuid>`.
//!
//! ## Environment variables (in addition to the turn object)
//!
//! | Variable                  | Default       | Purpose                                    |
//! |---------------------------|---------------|--------------------------------------------|
//! | `RECURSIVE_BIN`           | `recursive`   | Path to the binary (**≥ 0.8** recommended) |
//! | `RECURSIVE_WORKSPACE`     | (none)        | Workspace root the agent operates within   |
//! | `RECURSIVE_MODEL`         | (config)      | Override model (e.g. `claude-sonnet-4-5`)  |
//! | `RECURSIVE_PROVIDER`      | (config)      | Override provider (`openai` / `anthropic`) |
//! | `RECURSIVE_API_KEY`       | (config)      | API key (if not in ~/.recursive/config)    |
//! | `RECURSIVE_API_BASE`      | (config)      | Base URL for the LLM API endpoint          |
//! | `RECURSIVE_MAX_STEPS`     | (config)      | Max agent loop iterations                  |

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::{ChildStderr, Command};
use tokio::task::JoinHandle;

use super::common;

/// Claude Code–compatible NDJSON from `recursive --output-format stream-json`.
type RecursiveStreamEvent = common::StreamJsonEvent;

pub async fn run() -> Result<()> {
    let turn = common::read_turn_or_error();
    let (message, session_id) = common::message_and_session(&turn);

    let _new_session_id = common::with_session_resume_fallback(
        "recursive",
        &message,
        &session_id,
        |m, s| async move { stream_recursive(&m, &s).await },
    )
    .await?;

    Ok(())
}

/// Call `recursive --headless --output-format stream-json [-r <session_id>] -p <message>`,
/// emit each `assistant` text chunk as a `partial` event, the terminal
/// `result.result` as a `result` event (with `session_id`), and return the session
/// ID (from the `result` event, stderr UUID as fallback). On a failed `result`,
/// emit an `error` event and still return the session id so the bridge persists it.
async fn stream_recursive(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut emitter = common::SessionEmitter::new(session_id);
    let args = build_recursive_args(message, session_id, "stream-json");
    let bin = recursive_bin();

    let mut cmd = Command::new(&bin);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| {
        format!("failed to spawn `{bin}`; ensure recursive is installed and in PATH")
    })?;

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;
    let stderr_task = spawn_stderr_session_scanner(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut final_text: Option<String> = None;
    let mut got_any_output = false;
    let mut error_emitted = false;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read recursive stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<RecursiveStreamEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    let text = msg.text();
                    if !text.trim().is_empty() {
                        emitter.emit_partial(&text)?;
                        got_any_output = true;
                    }
                }
            }
            Some("result") => {
                let is_error = event.is_result_error();
                found_session_id = event.session_id.clone();
                if is_error {
                    if let Some(err_text) = event.result_error_message() {
                        emitter.emit_error(&err_text, found_session_id.as_deref())?;
                        error_emitted = true;
                        got_any_output = true;
                    }
                } else if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    final_text = Some(result_text);
                    got_any_output = true;
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for recursive")?;
    let (stderr, stderr_session_id) = stderr_task.await.unwrap_or_default();
    let resolved_session_id = prefer_session_id(found_session_id, stderr_session_id);

    // Emit the final reply body (result event). The bridge drops it in streaming
    // mode when partials already delivered the content (A1 dedup), and sends it
    // as the sole reply in non-streaming mode.
    if !error_emitted {
        emitter.emit_result_opt(final_text.as_deref(), resolved_session_id.as_deref())?;
    }

    common::ensure_success(
        "recursive",
        status,
        &stderr,
        got_any_output || resolved_session_id.is_some(),
    )?;

    Ok(resolved_session_id)
}

/// Build the CLI argument vector shared by streaming and one-shot modes.
fn build_recursive_args(message: &str, session_id: &str, output_format: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless".into(),
        "--output-format".into(),
        output_format.into(),
    ];

    // Optional model / provider / api-key overrides from environment.
    if let Ok(model) = std::env::var("RECURSIVE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }
    if let Ok(provider) = std::env::var("RECURSIVE_PROVIDER") {
        if !provider.trim().is_empty() {
            args.push("--provider".into());
            args.push(provider.trim().to_string());
        }
    }
    if let Ok(key) = std::env::var("RECURSIVE_API_KEY") {
        if !key.trim().is_empty() {
            args.push("--api-key".into());
            args.push(key.trim().to_string());
        }
    }
    if let Ok(base) = std::env::var("RECURSIVE_API_BASE") {
        if !base.trim().is_empty() {
            args.push("--api-base".into());
            args.push(base.trim().to_string());
        }
    }
    if let Ok(steps) = std::env::var("RECURSIVE_MAX_STEPS") {
        if !steps.trim().is_empty() {
            args.push("--max-steps".into());
            args.push(steps.trim().to_string());
        }
    }

    // Session resume: `-r <session_id>` routes to the saved session directory.
    if !session_id.is_empty() {
        args.push("-r".into());
        args.push(session_id.to_string());
        args.push("-p".into());
        args.push(message.to_string());
    } else {
        args.push("-p".into());
        args.push(message.to_string());
    }

    args
}

fn recursive_bin() -> String {
    std::env::var("RECURSIVE_BIN").unwrap_or_else(|_| "recursive".to_string())
}

fn prefer_session_id(from_result: Option<String>, from_stderr: Option<String>) -> Option<String> {
    from_result
        .filter(|s| !s.is_empty())
        .or(from_stderr.filter(|s| !s.is_empty()))
}

/// Parse `--output-format json` stdout: a single `result` object, or a JSON array
/// of events ending with `result` (Claude CLI ≥ 2.1.153 shape). Retained for
/// unit tests; the 0.3 agent always streams via `stream-json`.
#[allow(dead_code)]
fn parse_json_result(stdout: &str) -> Result<RecursiveStreamEvent> {
    let trimmed = stdout.trim();
    if trimmed.starts_with('[') {
        let events: Vec<RecursiveStreamEvent> = serde_json::from_str(trimmed)
            .with_context(|| format!("parse recursive json output: {stdout}"))?;
        events
            .into_iter()
            .find(|e| e.event_type.as_deref() == Some("result"))
            .ok_or_else(|| anyhow::anyhow!("no result event in recursive json output: {stdout}"))
    } else {
        serde_json::from_str(trimmed)
            .with_context(|| format!("parse recursive json output: {stdout}"))
    }
}

fn spawn_stderr_session_scanner(child_stderr: ChildStderr) -> JoinHandle<(String, Option<String>)> {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(child_stderr);
        let mut line = String::new();
        let mut full_stderr = String::new();
        let mut captured_session_id: Option<String> = None;
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if captured_session_id.is_none() {
                if let Some(uuid) = extract_session_id_from_stderr(&line) {
                    captured_session_id = Some(uuid);
                }
            }
            if full_stderr.len() < crate::bridge::MAX_CLI_CAPTURE_BYTES {
                full_stderr.push_str(&line);
            }
        }
        (full_stderr, captured_session_id)
    })
}

/// Extract a session UUID from a `recursive` stderr progress line.
fn extract_session_id_from_stderr(line: &str) -> Option<String> {
    let trimmed = line.trim();

    if !trimmed.starts_with("session:") {
        return None;
    }
    let to_pos = trimmed.find(" to ")?;
    let path_part = trimmed[to_pos + 4..].trim();

    let path_no_slash = path_part.trim_end_matches('/');
    let uuid = path_no_slash.rsplit('/').next().filter(|s| !s.is_empty())?;

    if uuid.len() >= 32 && uuid.contains('-') {
        Some(uuid.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_session_id_recording() {
        let line = "session: recording to /Users/kongjie/.recursive/workspaces/abc123/sessions/my-goal/550e8400-e29b-41d4-a716-446655440000/";
        let id = extract_session_id_from_stderr(line).unwrap();
        assert_eq!(id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn extract_session_id_appending() {
        let line = "session: appending to /home/user/.recursive/sessions/slug/550e8400-e29b-41d4-a716-446655440000/";
        let id = extract_session_id_from_stderr(line).unwrap();
        assert_eq!(id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn extract_session_id_saved() {
        let line = "session: saved 8 message(s) to /home/user/.recursive/sessions/slug/550e8400-e29b-41d4-a716-446655440000/";
        let id = extract_session_id_from_stderr(line).unwrap();
        assert_eq!(id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn extract_session_id_unrelated_line_returns_none() {
        assert!(extract_session_id_from_stderr("checkpoint: per-turn snapshots active").is_none());
        assert!(extract_session_id_from_stderr("").is_none());
    }

    #[test]
    fn deserialize_assistant_event() {
        let json =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello!"}]}}"#;
        let event: RecursiveStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("assistant"));
        assert_eq!(event.message.as_ref().unwrap().text(), "Hello!");
    }

    #[test]
    fn deserialize_result_event() {
        let json = r#"{"type":"result","result":"Hello!","session_id":"550e8400-e29b-41d4-a716-446655440000"}"#;
        let event: RecursiveStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("result"));
        assert_eq!(event.result.as_deref(), Some("Hello!"));
        assert_eq!(
            event.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn parse_json_result_single_object() {
        let stdout = r#"{"type":"result","result":"done","session_id":"sess-1"}"#;
        let event = parse_json_result(stdout).unwrap();
        assert_eq!(event.result.as_deref(), Some("done"));
        assert_eq!(event.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn parse_json_result_from_event_array() {
        let stdout = r#"[{"type":"system","subtype":"init"},{"type":"result","result":"done","session_id":"sess-2"}]"#;
        let event = parse_json_result(stdout).unwrap();
        assert_eq!(event.result.as_deref(), Some("done"));
        assert_eq!(event.session_id.as_deref(), Some("sess-2"));
    }

    #[test]
    fn prefer_session_id_favors_result_over_stderr() {
        assert_eq!(
            prefer_session_id(
                Some("from-result".into()),
                Some("550e8400-e29b-41d4-a716-446655440000".into())
            )
            .as_deref(),
            Some("from-result")
        );
    }

    #[test]
    fn prefer_session_id_falls_back_to_stderr() {
        assert_eq!(
            prefer_session_id(None, Some("550e8400-e29b-41d4-a716-446655440000".into())).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn deserialize_result_error_event() {
        let json = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["LLM error: HTTP 404"],"stop_reason":"provider_stop:LLM error: HTTP 404","session_id":"sess-err"}"#;
        let event: RecursiveStreamEvent = serde_json::from_str(json).unwrap();
        assert!(event.is_result_error());
        assert_eq!(
            event.result_error_message().as_deref(),
            Some("LLM error: HTTP 404")
        );
    }

    #[test]
    fn build_args_stream_mode() {
        let args = build_recursive_args("hello", "", "stream-json");
        assert_eq!(args[0], "--headless");
        assert_eq!(args[1], "--output-format");
        assert_eq!(args[2], "stream-json");
        assert!(!args.iter().any(|a| a == "-r"));
        let p_pos = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[p_pos + 1], "hello");
    }

    #[test]
    fn build_args_oneshot_uses_json_format() {
        let args = build_recursive_args("hello", "", "json");
        assert_eq!(args[2], "json");
    }

    #[test]
    fn build_args_resume_session_includes_resume_flag() {
        let args = build_recursive_args(
            "next msg",
            "550e8400-e29b-41d4-a716-446655440000",
            "stream-json",
        );
        let r_pos = args.iter().position(|a| a == "-r").unwrap();
        assert_eq!(args[r_pos + 1], "550e8400-e29b-41d4-a716-446655440000");
        let p_pos = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[p_pos + 1], "next msg");
        assert!(r_pos < p_pos);
    }
}
