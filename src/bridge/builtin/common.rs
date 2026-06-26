//! Shared helpers for the built-in profile handlers.
//!
//! Every built-in (`claude-code`, `codex`, `cursor`, `agy`) follows the same P0
//! exec protocol: read `AGENT_MESSAGE` / `AGENT_SESSION_ID`, run the underlying
//! CLI (resuming the session when one exists, falling back to a fresh session on
//! failure), stream partials, and finally print `AGENT_SESSION:<id>`. This module
//! factors out that boilerplate so each handler only carries its CLI-specific glue.

use std::future::Future;
use std::process::ExitStatus;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::AsyncRead;
use tokio::task::JoinHandle;

/// Read the two P0 env vars injected by the bridge: the inbound user message and
/// the existing session id (empty string = no session yet).
pub fn read_message_and_session() -> (String, String) {
    let message = std::env::var("AGENT_MESSAGE").unwrap_or_default();
    let session_id = std::env::var("AGENT_SESSION_ID").unwrap_or_default();
    (message, session_id)
}

/// Run `op(message, session_id)`. When `session_id` is non-empty (a resume attempt)
/// and it fails, log the error and retry once as a fresh session so the user gets a
/// response rather than a bare error. With an empty `session_id` the op runs once.
///
/// Generic over the op's success type so it works for both the streaming handlers
/// (which return `Option<String>`) and `agy` (which returns `(String, Option<String>)`).
pub async fn with_session_resume_fallback<T, F, Fut>(
    tool: &str,
    message: &str,
    session_id: &str,
    op: F,
) -> Result<T>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    if session_id.is_empty() {
        return op(message.to_string(), String::new()).await;
    }

    match op(message.to_string(), session_id.to_string()).await {
        Ok(v) => Ok(v),
        Err(e) => {
            eprintln!("[{tool}] resume failed ({e:#}), retrying as new session");
            op(message.to_string(), String::new()).await
        }
    }
}

/// Print the final P0 session line (`AGENT_SESSION:<id>`) when an id is present and
/// non-empty. A no-op otherwise.
pub fn emit_session_line(session_id: Option<&str>) {
    if let Some(sid) = session_id {
        if !sid.is_empty() {
            println!("AGENT_SESSION:{sid}");
        }
    }
}

/// Emit one streamed chunk as a P0 partial line (`AGENT_PARTIAL:<json-string>`) and
/// flush stdout so the bridge forwards it immediately.
pub fn emit_partial(text: &str) -> Result<()> {
    println!("AGENT_PARTIAL:{}", serde_json::to_string(text)?);
    std::io::Write::flush(&mut std::io::stdout()).ok();
    Ok(())
}

/// Spawn a background task that drains a child pipe (typically stderr) into a String,
/// capped at [`crate::bridge::MAX_CLI_CAPTURE_BYTES`]. Draining concurrently prevents a
/// full pipe buffer from deadlocking the child; the cap prevents a runaway CLI from
/// growing the buffer without bound (OOM guard).
pub fn spawn_capped_drain<R>(reader: R) -> JoinHandle<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, BufReader};
        let mut buf = Vec::new();
        BufReader::new(reader)
            .take(crate::bridge::MAX_CLI_CAPTURE_BYTES as u64)
            .read_to_end(&mut buf)
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Fail with a descriptive error when the CLI exited unsuccessfully and produced no
/// usable result. `recovered` should be true when a session id / response was still
/// obtained (in which case a non-zero exit is tolerated).
pub fn ensure_success(tool: &str, status: ExitStatus, stderr: &str, recovered: bool) -> Result<()> {
    if !status.success() && !recovered {
        let detail = if stderr.is_empty() {
            "(no output)"
        } else {
            stderr
        };
        anyhow::bail!(
            "{tool} exited with status {:?}\nstderr: {detail}",
            status.code()
        );
    }
    Ok(())
}

/// JSON shape of a single event line from the `stream-json` output format shared by
/// the Claude Code CLI and the Cursor `agent` CLI (Cursor adopted the same schema):
/// `type == "assistant"` carries incremental text, `type == "result"` carries the
/// final `session_id` and `result` text.
#[derive(Debug, Deserialize)]
pub struct StreamJsonEvent {
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    /// Final result text (present on `type == "result"` events).
    pub result: Option<String>,
    /// Session ID (present on `type == "result"` events).
    pub session_id: Option<String>,
    /// Present on `type == "result"` events (`"success"` or an error subtype).
    #[allow(dead_code)]
    pub subtype: Option<String>,
    /// Present on `type == "assistant"` events.
    pub message: Option<StreamMessage>,
}

/// Nested message structure in `type == "assistant"` stream events.
#[derive(Debug, Deserialize)]
pub struct StreamMessage {
    pub content: Option<Vec<StreamContentBlock>>,
}

impl StreamMessage {
    /// Concatenate the text of every `text` content block, ignoring non-text blocks
    /// (e.g. `tool_use`, `image`). Multimodal replies can interleave block types.
    pub fn text(&self) -> String {
        self.content
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|b| b.block_type.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A single content block within a [`StreamMessage`].
#[derive(Debug, Deserialize)]
pub struct StreamContentBlock {
    #[serde(rename = "type")]
    pub block_type: Option<String>,
    pub text: Option<String>,
}
