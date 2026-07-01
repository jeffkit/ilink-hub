//! Built-in `recursive` profile: wraps the `recursive` CLI with session continuity.
//!
//! Reads P0 env vars, calls `recursive --headless --output-format stream-json
//! [-r <session_id>] -p <message>`, and streams assistant text output to the
//! parent bridge via `AGENT_PARTIAL:` stdout lines.
//!
//! ## Output parsing
//!
//! `recursive --output-format stream-json` emits newline-delimited JSON events
//! tagged with `#[serde(tag = "type", rename_all = "snake_case")]`:
//!
//! ```json
//! {"type":"assistant_text","text":"hello","step":1}
//! {"type":"partial_token","text":"hel","step":1}
//! {"type":"turn_finished","reason":"no_more_tool_calls","steps":3}
//! ```
//!
//! Only `assistant_text` events carry the final reply text. `partial_token` events
//! carry live streaming deltas (available when `--stream` is also set, but we don't
//! use `--stream` to keep output deterministic and avoid duplicate partials).
//!
//! ## Session continuity
//!
//! `recursive` persists sessions as JSONL directories under
//! `~/.recursive/workspaces/<hash>/sessions/<slug>/<session-uuid>/`. The session
//! UUID is emitted on stderr as:
//!
//! ```text
//! session: recording to /path/to/sessions/<slug>/<uuid>/
//! ```
//!
//! We capture that path from stderr and store the UUID as the Hub session ID.
//! Resume is requested with `-r <uuid>` which the CLI resolves back to the full
//! path via `recursive::user_sessions_dir`.
//!
//! ## Environment variables (in addition to standard P0 vars)
//!
//! | Variable                  | Default       | Purpose                                    |
//! |---------------------------|---------------|--------------------------------------------|
//! | `RECURSIVE_WORKSPACE`     | (none)        | Workspace root the agent operates within   |
//! | `RECURSIVE_MODEL`         | (config)      | Override model (e.g. `claude-sonnet-4-5`)  |
//! | `RECURSIVE_PROVIDER`      | (config)      | Override provider (`openai` / `anthropic`) |
//! | `RECURSIVE_API_KEY`       | (config)      | API key (if not in ~/.recursive/config)    |
//! | `RECURSIVE_API_BASE`      | (config)      | Base URL for the LLM API endpoint          |
//! | `RECURSIVE_MAX_STEPS`     | (config)      | Max agent loop iterations                  |

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

use super::common;

/// A single JSON event emitted on stdout by `recursive --output-format stream-json`.
/// Uses `tag = "type"` with `rename_all = "snake_case"` to match recursive's schema.
#[derive(Debug, Deserialize)]
struct RecursiveEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    /// Present on `type == "assistant_text"`.
    text: Option<String>,
}

pub async fn run() -> Result<()> {
    let (message, session_id) = common::read_message_and_session();

    let new_session_id = common::with_session_resume_fallback(
        "recursive",
        &message,
        &session_id,
        |m, s| async move { stream_recursive(&m, &s).await },
    )
    .await?;

    common::emit_session_line(new_session_id.as_deref());

    Ok(())
}

/// Call `recursive --headless --output-format stream-json [-r <session_id>] -p <message>`,
/// emit each `assistant_text` event as an `AGENT_PARTIAL:` stdout line, and return
/// the session UUID extracted from the stderr progress line.
async fn stream_recursive(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut args: Vec<String> = vec![
        "--headless".into(),
        "--output-format".into(),
        "stream-json".into(),
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
        // When resuming, pass the new message via -p; the CLI appends it to the
        // existing transcript.
        args.push("-p".into());
        args.push(message.to_string());
    } else {
        // Fresh session: one-shot `-p <message>`.
        args.push("-p".into());
        args.push(message.to_string());
    }

    // Allow overriding the binary path via RECURSIVE_BIN so that brew-installed
    // builds (/opt/homebrew/bin/recursive) are found even when the hub process
    // runs with a minimal PATH that omits /opt/homebrew/bin.
    let bin = std::env::var("RECURSIVE_BIN").unwrap_or_else(|_| "recursive".to_string());

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

    // Drain stderr in background; also scan for the session path line.
    let stderr_task = tokio::spawn(async move {
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
            // Extract session UUID from lines like:
            //   "session: recording to /path/to/sessions/<slug>/<uuid>/"
            //   "session: appending to /path/to/sessions/<slug>/<uuid>/"
            //   "session: saved N message(s) to /path/to/sessions/<slug>/<uuid>/"
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
    });

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut got_any_output = false;

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

        let Ok(event) = serde_json::from_str::<RecursiveEvent>(trimmed) else {
            continue;
        };

        if event.event_type.as_deref() == Some("assistant_text") {
            if let Some(text) = &event.text {
                if !text.trim().is_empty() {
                    common::emit_partial(text)?;
                    got_any_output = true;
                }
            }
        }
    }

    let status = child.wait().await.context("wait for recursive")?;
    let (stderr, captured_session_id) = stderr_task.await.unwrap_or_default();

    common::ensure_success(
        "recursive",
        status,
        &stderr,
        got_any_output || captured_session_id.is_some(),
    )?;

    Ok(captured_session_id)
}

/// Extract a session UUID from a `recursive` stderr progress line.
///
/// Handles both formats:
/// - `session: recording to /…/sessions/<slug>/<uuid>/`
/// - `session: appending to /…/sessions/<slug>/<uuid>/`
/// - `session: saved 4 message(s) to /…/sessions/<slug>/<uuid>/`
///
/// Returns the final path component (stripped of trailing slash) which is the UUID.
fn extract_session_id_from_stderr(line: &str) -> Option<String> {
    let trimmed = line.trim();

    // Match lines that start with "session:" and contain " to "
    if !trimmed.starts_with("session:") {
        return None;
    }
    let to_pos = trimmed.find(" to ")?;
    let path_part = trimmed[to_pos + 4..].trim();

    // The path ends with the session UUID directory, possibly with a trailing slash.
    let path_no_slash = path_part.trim_end_matches('/');
    let uuid = path_no_slash.rsplit('/').next().filter(|s| !s.is_empty())?;

    // Basic sanity check: session IDs are UUID-like (contains hyphens, ≥ 32 chars).
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
    fn extract_session_id_no_trailing_slash() {
        let line = "session: recording to /home/user/.recursive/sessions/slug/550e8400-e29b-41d4-a716-446655440000";
        let id = extract_session_id_from_stderr(line).unwrap();
        assert_eq!(id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn extract_session_id_unrelated_line_returns_none() {
        assert!(extract_session_id_from_stderr("checkpoint: per-turn snapshots active").is_none());
        assert!(extract_session_id_from_stderr("warning: legacy in-tree state detected").is_none());
        assert!(extract_session_id_from_stderr("").is_none());
    }

    #[test]
    fn extract_session_id_short_path_component_returns_none() {
        // A path ending in a slug (not a UUID) should not be mistaken for a session.
        let line = "session: recording to /home/user/.recursive/sessions/short-name/";
        assert!(extract_session_id_from_stderr(line).is_none());
    }

    #[test]
    fn deserialize_assistant_text_event() {
        let json = r#"{"type":"assistant_text","text":"Hello from recursive!","step":1}"#;
        let event: RecursiveEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("assistant_text"));
        assert_eq!(event.text.as_deref(), Some("Hello from recursive!"));
    }

    #[test]
    fn deserialize_turn_finished_event() {
        let json = r#"{"type":"turn_finished","reason":"no_more_tool_calls","steps":3}"#;
        let event: RecursiveEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("turn_finished"));
        assert!(event.text.is_none());
    }

    #[test]
    fn deserialize_unknown_event_does_not_panic() {
        let json = r#"{"type":"tool_call","name":"read_file","arguments":"{}","step":2}"#;
        let event: RecursiveEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("tool_call"));
    }
}
