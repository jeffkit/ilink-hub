//! Shared helpers for the built-in profile handlers.
//!
//! Every built-in (`claude-code`, `codex`, `cursor`, `agy`, `recursive`) is an
//! agentproc 0.4 **agent**: it reads the NDJSON [`TurnInput`](crate::bridge::protocol::TurnInput)
//! from stdin, runs the underlying CLI (resuming the session when one exists,
//! falling back to a fresh session on failure), streams `partial` events, and
//! finally emits a single `result` event (with optional `session_id`). This
//! module factors out that boilerplate so each handler only carries its
//! CLI-specific glue.

use std::future::Future;
use std::process::ExitStatus;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::AsyncRead;
use tokio::task::JoinHandle;

use crate::bridge::protocol::{self, TurnInput};

/// Read the turn object the bridge wrote to stdin. On a missing/empty turn,
/// emits an `error` event and returns a tuple with an empty message/session so
/// the caller can exit cleanly.
pub fn read_turn_or_error() -> TurnInput {
    match protocol::read_turn() {
        Some(turn) if turn.has_content() => turn,
        Some(_) => {
            emit_error("turn is empty (no message and no attachments)");
            TurnInput::default()
        }
        None => {
            emit_error("no turn object on stdin");
            TurnInput::default()
        }
    }
}

/// Backwards-compat accessor pair for handlers that only need message + session.
pub fn message_and_session(turn: &TurnInput) -> (String, String) {
    (turn.message.clone(), turn.session_id.clone())
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

/// Emit one streamed chunk as a `{"type":"partial","text":...}` event and flush
/// stdout so the bridge forwards it immediately.
pub fn emit_partial_with_session(text: &str, session_id: Option<&str>) -> Result<()> {
    let mut obj = serde_json::json!({"type":"partial","text":text});
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        obj["session_id"] = serde_json::json!(sid);
    }
    println!("{obj}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    Ok(())
}

/// Emit the terminal `{"type":"result","text":...}` success body (at most one
/// per turn). When `session_id` is present and non-empty it is attached for
/// continuity; otherwise the field is omitted (stateless agents).
pub fn emit_result(text: &str, session_id: Option<&str>) -> Result<()> {
    let mut obj = serde_json::json!({"type":"result","text":text});
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        obj["session_id"] = serde_json::json!(sid);
    }
    println!("{obj}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    Ok(())
}

/// Emit `result` when there is a body and/or a session id to report. No-op when
/// both are empty (nothing to persist or show).
pub fn emit_result_or_session(text: Option<&str>, session_id: Option<&str>) -> Result<()> {
    let body = text.unwrap_or("");
    let has_sid = session_id.is_some_and(|s| !s.is_empty());
    if body.is_empty() && !has_sid {
        return Ok(());
    }
    emit_result(body, session_id)
}

/// Emit a terminal `{"type":"error","message":...}` event. The bridge forwards
/// the message to the user and marks the turn failed. The agent SHOULD exit
/// non-zero shortly after; this helper does not exit for the caller.
pub fn emit_error(text: &str) {
    emit_error_with_session(text, None);
}

/// Like [`emit_error`], optionally attaching `session_id` so a failed turn can
/// still hand back continuity (0.4).
pub fn emit_error_with_session(text: &str, session_id: Option<&str>) {
    let mut obj = serde_json::json!({"type":"error","message":text});
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        obj["session_id"] = serde_json::json!(sid);
    }
    println!("{obj}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
}

/// Helper that stamps `session_id` on AgentProc 0.4 events.
///
/// - When the inbound turn already carries a session id (resume), every event
///   is stamped immediately (spec SHOULD).
/// - When starting a new session, `partial`s are buffered until the CLI
///   discovers an id, then flushed with that id. `permission_request` must not
///   be buffered (callers emit those directly). If the turn ends with no id
///   (stateless), buffered partials are flushed without `session_id`.
pub struct SessionEmitter {
    inbound: String,
    discovered: Option<String>,
    buffered_partials: Vec<String>,
}

impl SessionEmitter {
    pub fn new(inbound_session_id: &str) -> Self {
        Self {
            inbound: inbound_session_id.to_string(),
            discovered: None,
            buffered_partials: Vec::new(),
        }
    }

    /// Effective id to stamp: inbound (resume) wins until/unless discovery
    /// fills in a new-session id.
    pub fn current_id(&self) -> Option<&str> {
        if !self.inbound.is_empty() {
            Some(self.inbound.as_str())
        } else {
            self.discovered.as_deref().filter(|s| !s.is_empty())
        }
    }

    /// Record a CLI-discovered session id (no-op when empty or when inbound
    /// already provides continuity). Flushes any buffered partials.
    pub fn discover(&mut self, id: Option<&str>) -> Result<()> {
        let Some(id) = id.filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        if self.inbound.is_empty() && self.discovered.is_none() {
            self.discovered = Some(id.to_string());
            self.flush_partials()?;
        }
        Ok(())
    }

    pub fn emit_partial(&mut self, text: &str) -> Result<()> {
        if let Some(sid) = self.current_id() {
            emit_partial_with_session(text, Some(sid))
        } else {
            // New session, id not yet known — buffer.
            self.buffered_partials.push(text.to_string());
            Ok(())
        }
    }

    pub fn emit_result_opt(&mut self, text: Option<&str>, session_id: Option<&str>) -> Result<()> {
        self.discover(session_id)?;
        let sid = self
            .current_id()
            .map(str::to_string)
            .or_else(|| session_id.filter(|s| !s.is_empty()).map(str::to_string));
        self.flush_partials()?;
        emit_result_or_session(text.filter(|t| !t.is_empty()), sid.as_deref())
    }

    pub fn emit_error(&mut self, text: &str, session_id: Option<&str>) -> Result<()> {
        self.discover(session_id)?;
        let sid = self
            .current_id()
            .map(str::to_string)
            .or_else(|| session_id.filter(|s| !s.is_empty()).map(str::to_string));
        // Flush buffered partials before the terminal error so ordering is preserved.
        self.flush_partials()?;
        emit_error_with_session(text, sid.as_deref());
        Ok(())
    }

    /// Flush remaining buffered partials without a session id (stateless end).
    pub fn finish_without_session(&mut self) -> Result<()> {
        self.flush_partials()
    }

    fn flush_partials(&mut self) -> Result<()> {
        if self.buffered_partials.is_empty() {
            return Ok(());
        }
        let sid = self.current_id().map(str::to_string);
        let pending = std::mem::take(&mut self.buffered_partials);
        for text in pending {
            emit_partial_with_session(&text, sid.as_deref())?;
        }
        Ok(())
    }
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
    pub subtype: Option<String>,
    /// Present on terminal `result` events when the run failed.
    pub is_error: Option<bool>,
    /// Human-readable error strings on failed `result` events (recursive / Claude CLI).
    pub errors: Option<Vec<String>>,
    /// Provider stop reason on terminal `result` events.
    pub stop_reason: Option<String>,
    /// Present on `type == "assistant"` events.
    pub message: Option<StreamMessage>,
}

impl StreamJsonEvent {
    /// Whether this terminal `result` event reports a failed run.
    pub fn is_result_error(&self) -> bool {
        self.is_error == Some(true) || self.subtype.as_deref() == Some("error_during_execution")
    }

    /// Best-effort error text from a failed terminal `result` event.
    pub fn result_error_message(&self) -> Option<String> {
        if !self.is_result_error() {
            return None;
        }
        if let Some(errors) = &self.errors {
            let joined = errors
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
        // `stop_sequence` is a normal model stop reason (the model hit a stop
        // marker), not a user-visible error.  Exposing it verbatim confuses
        // users (they see a bare "stop_sequence" reply).  Other stop_reason
        // values such as "provider_stop:…" do carry useful diagnostic info and
        // are kept.
        if let Some(reason) = self
            .stop_reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| *s != "stop_sequence")
        {
            return Some(reason.to_string());
        }
        self.result
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── StreamMessage::text() ─────────────────────────────────────────────────

    #[test]
    fn stream_message_text_concatenates_text_blocks() {
        let msg = StreamMessage {
            content: Some(vec![
                StreamContentBlock {
                    block_type: Some("text".into()),
                    text: Some("Hello".into()),
                },
                StreamContentBlock {
                    block_type: Some("tool_use".into()),
                    text: Some("ignored".into()),
                },
                StreamContentBlock {
                    block_type: Some("text".into()),
                    text: Some(" World".into()),
                },
            ]),
        };
        assert_eq!(msg.text(), "Hello World");
    }

    #[test]
    fn stream_message_text_ignores_non_text_block_types() {
        let msg = StreamMessage {
            content: Some(vec![
                StreamContentBlock {
                    block_type: Some("image".into()),
                    text: Some("should be ignored".into()),
                },
                StreamContentBlock {
                    block_type: Some("tool_use".into()),
                    text: Some("also ignored".into()),
                },
            ]),
        };
        assert_eq!(
            msg.text(),
            "",
            "non-text blocks must not contribute to output"
        );
    }

    #[test]
    fn stream_message_text_empty_when_no_content() {
        let msg = StreamMessage { content: None };
        assert_eq!(msg.text(), "");
    }

    #[test]
    fn stream_message_text_skips_text_block_with_none_text_field() {
        let msg = StreamMessage {
            content: Some(vec![StreamContentBlock {
                block_type: Some("text".into()),
                text: None,
            }]),
        };
        assert_eq!(
            msg.text(),
            "",
            "None text field in text block must produce empty output"
        );
    }

    #[test]
    fn result_error_message_prefers_errors_array() {
        let json = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["LLM 404"],"stop_reason":"provider_stop:LLM 404","session_id":"sess-1"}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert!(event.is_result_error());
        assert_eq!(event.result_error_message().as_deref(), Some("LLM 404"));
    }

    #[test]
    fn result_error_message_falls_back_to_stop_reason() {
        let json = r#"{"type":"result","is_error":true,"stop_reason":"provider_stop:boom","session_id":"sess-1"}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            event.result_error_message().as_deref(),
            Some("provider_stop:boom")
        );
    }

    #[test]
    fn result_error_message_suppresses_stop_sequence() {
        // GLM (and other models via Anthropic-compat) emit stop_reason="stop_sequence"
        // when the model hits a stop marker.  This is a normal stop, not a real
        // error; the bridge must not forward it as a user-visible reply.
        let json = r#"{"type":"result","is_error":true,"stop_reason":"stop_sequence","session_id":"sess-1"}"#;
        let event: StreamJsonEvent = serde_json::from_str(json).unwrap();
        assert!(
            event.is_result_error(),
            "is_error=true must still be detected as error"
        );
        assert_eq!(
            event.result_error_message(),
            None,
            "stop_sequence must not produce a user-visible error message"
        );
    }

    // ── ensure_success ────────────────────────────────────────────────────────

    #[cfg(unix)]
    mod unix_tests {
        use super::*;
        use std::os::unix::process::ExitStatusExt;

        fn exit(code: i32) -> std::process::ExitStatus {
            // On UNIX the raw status is encoded as (exit_code << 8).
            std::process::ExitStatus::from_raw(code << 8)
        }

        #[test]
        fn ensure_success_ok_for_status_zero() {
            assert!(ensure_success("tool", exit(0), "", false).is_ok());
        }

        #[test]
        fn ensure_success_err_when_nonzero_and_not_recovered() {
            let result = ensure_success("tool", exit(1), "stderr output", false);
            assert!(
                result.is_err(),
                "non-zero exit without recovery must be Err"
            );
        }

        #[test]
        fn ensure_success_ok_when_nonzero_but_recovered() {
            // A non-zero exit is tolerated when a usable session/response was obtained.
            assert!(
                ensure_success("tool", exit(1), "stderr", true).is_ok(),
                "recovered=true must suppress non-zero exit error"
            );
        }

        #[test]
        fn ensure_success_err_message_includes_tool_name() {
            let err = ensure_success("my-tool", exit(2), "", false).unwrap_err();
            assert!(
                err.to_string().contains("my-tool"),
                "error must include tool name"
            );
        }
    }
}
