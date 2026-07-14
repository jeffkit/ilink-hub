//! AgentProc wire protocol 0.3 — NDJSON in both directions.
//!
//! - **stdin**: the bridge writes exactly one [`TurnObject`] line, then EOF
//!   (unless `permission: true`, in which case stdin stays open for
//!   [`PermissionResponse`] frames).
//! - **stdout**: the agent emits one JSON object per line, distinguished by a
//!   `type` field from a closed vocabulary: `partial` / `text` / `session` /
//!   `error` / `permission_request`. Unknown or malformed lines are logged and
//!   ignored — they are NOT treated as reply body (this is the 0.2→0.3 cutover).
//!
//! See `docs/knowledge/bridges/profile-protocol.md` and the upstream spec at
//! `~/projects/agentproc/spec/protocol.md`.

use serde::{Deserialize, Serialize};

/// Wire-protocol version string carried in the turn object. Opaque and
/// non-comparable per the spec — agents MUST NOT order or range-check it.
pub const PROTOCOL_VERSION: &str = "0.3";

/// One element of the turn object's `attachments` array.
///
/// The bridge builds this from a WeChat message's media items. `kind` is
/// `"image"`, `"file"`, or `"video"`; `url` is the CDN URL the agent fetches.
/// Additional fields (`filename`, `mime_type`, `size`) are forwarded when known.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
}

/// The turn object the bridge writes to the agent's stdin as a single NDJSON
/// line before the process reads its first byte.
///
/// Required fields are always emitted; optional fields use presence-as-feature
/// (`permission` is omitted when false, `attachments` is emitted as `[]` when
/// the turn carries no media, `session_name` defaults to `"default"`).
#[derive(Debug, Clone, Serialize)]
pub struct TurnObject {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub message: String,
    pub session_id: String,
    pub from_user: String,
    pub protocol_version: &'static str,
    pub session_name: String,
    pub attachments: Vec<Attachment>,
    /// Included (true) only when the profile enables the permission channel.
    #[serde(skip_serializing_if = "is_false")]
    pub permission: bool,
}

impl TurnObject {
    /// Build a turn object for this turn. `permission` is emitted on the wire
    /// only when true (presence-as-feature).
    pub fn new(
        message: impl Into<String>,
        session_id: impl Into<String>,
        session_name: impl Into<String>,
        from_user: impl Into<String>,
        attachments: Vec<Attachment>,
        permission: bool,
    ) -> Self {
        Self {
            event_type: "turn",
            message: message.into(),
            session_id: session_id.into(),
            from_user: from_user.into(),
            protocol_version: PROTOCOL_VERSION,
            session_name: session_name.into(),
            attachments,
            permission,
        }
    }

    /// Serialize as a single NDJSON line (no trailing newline).
    pub fn to_ndjson(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

/// The turn object as read by an **agent** from its stdin (deserialized).
///
/// This mirrors [`TurnObject`] but is deserializable and tolerant of missing
/// optional fields (`session_name` defaults to `"default"`, `attachments` to
/// `[]`, `permission` to `false`). Built-in profiles use [`read_turn`] to
/// consume it.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TurnInput {
    #[serde(rename = "type", default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default = "default_session_name")]
    pub session_name: String,
    #[serde(default)]
    pub from_user: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub permission: bool,
    #[serde(default)]
    pub protocol_version: String,
}

fn default_session_name() -> String {
    "default".to_string()
}

impl TurnInput {
    /// Whether this turn carries any user content (text or attachments). Per
    /// the spec, a turn with neither is empty and the agent should error.
    pub fn has_content(&self) -> bool {
        !self.message.is_empty() || !self.attachments.is_empty()
    }
}

/// Read exactly one NDJSON line (the turn object) from stdin. Used by built-in
/// profile handlers running as agent subprocesses.
///
/// Returns `None` on EOF or malformed JSON. Callers should emit an `error`
/// event and exit non-zero when the turn is missing or empty (no message and
/// no attachments).
pub fn read_turn() -> Option<TurnInput> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut handle = stdin.lock();
    if handle.read_line(&mut line).ok()? == 0 {
        return None;
    }
    serde_json::from_str::<TurnInput>(line.trim()).ok()
}

/// Distinguish assistant output from reasoning/thinking text on `partial` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PartialRole {
    #[default]
    Output,
    Thinking,
}

/// A tool-permission request emitted by the agent (only when `permission: true`).
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// A parsed stdout event from the agent. Unknown / malformed lines do not
/// produce a variant; [`parse_event`] returns `None` for them so the caller
/// logs and ignores per the spec's "malformed lines" rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A streaming chunk, forwarded to the user immediately (when streaming).
    Partial {
        text: String,
        #[serde(default)]
        role: Option<PartialRole>,
    },
    /// A piece of the final reply body. Multiple `text` events concatenate.
    Text { text: String },
    /// Declares or updates the session id. Last one wins.
    Session { id: String },
    /// A terminal error message forwarded to the user.
    Error { message: String },
    /// Optional tool-permission request (only when `permission: true`).
    PermissionRequest(PermissionRequest),
}

/// Parse one stdout line into a typed [`AgentEvent`].
///
/// Returns:
/// - `Ok(Some(event))` for a recognised, well-formed event.
/// - `Ok(None)` for an unknown `type`, a non-object JSON value, or malformed
///   JSON — the caller SHOULD log a warning and ignore the line per the spec.
/// - `Err(_)` never (malformed input maps to `None`).
pub fn parse_event(line: &str) -> Option<AgentEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Two-step parse: first a Value to detect the type without failing on
    // unknown variants, then delegate to the tagged enum for the known ones.
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let obj = value.as_object()?;
    let ty = obj.get("type").and_then(|v| v.as_str())?;
    // Re-deserialize through the tagged enum so field validation is centralised.
    // `type` values outside the closed vocabulary fail the enum and yield None.
    let _ = ty;
    serde_json::from_value::<AgentEvent>(value).ok()
}

/// A permission response the bridge writes to the agent's stdin as one NDJSON
/// line (only when `permission: true`).
#[derive(Debug, Clone, Serialize)]
pub struct PermissionResponse {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub request_id: String,
    pub behavior: PermissionBehavior,
    /// Present when `behavior` is `Allow` and the bridge wants to override the
    /// tool input (e.g. Claude Code's `updatedInput`). Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    /// Present when `behavior` is `Deny`, carrying a reason the agent may surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PermissionResponse {
    pub fn allow(request_id: impl Into<String>) -> Self {
        Self {
            event_type: "permission_response",
            request_id: request_id.into(),
            behavior: PermissionBehavior::Allow,
            updated_input: None,
            message: None,
        }
    }

    pub fn deny(request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            event_type: "permission_response",
            request_id: request_id.into(),
            behavior: PermissionBehavior::Deny,
            updated_input: None,
            message: Some(reason.into()),
        }
    }

    pub fn to_ndjson(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

/// The bridge's default action when a `permission_request` arrives.
///
/// `Ask` pauses the turn and prompts the user over WeChat to allow/deny the
/// tool call (the interactive approval loop lives in the dispatcher's
/// `ApprovalBroker` and the executor's ask handling).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDefaultPolicy {
    /// Auto-approve every tool call. Equivalent to `--dangerously-skip-permissions`.
    #[default]
    Allow,
    /// Deny every tool call with a reason; the agent must do without the tool.
    Deny,
    /// Log the request and deny (safe default for auditing without blocking).
    DenyLogged,
    /// Pause the turn and ask the user to approve/deny over WeChat. Falls back
    /// to `Deny` if no interactive broker is wired up (e.g. in tests/probe).
    Ask,
}

fn is_false(b: &bool) -> bool {
    !b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_object_serializes_required_and_optional_fields() {
        let turn = TurnObject::new("hi", "", "default", "u1", vec![], false);
        let json = turn.to_ndjson().unwrap();
        // permission=false is skipped (presence-as-feature).
        assert!(!json.contains("\"permission\""));
        assert!(json.contains("\"type\":\"turn\""));
        assert!(json.contains("\"message\":\"hi\""));
        assert!(json.contains("\"session_id\":\"\""));
        assert!(json.contains("\"from_user\":\"u1\""));
        assert!(json.contains("\"protocol_version\":\"0.3\""));
        assert!(json.contains("\"session_name\":\"default\""));
        assert!(json.contains("\"attachments\":[]"));
    }

    #[test]
    fn turn_object_includes_permission_when_true() {
        let turn = TurnObject::new("hi", "s1", "feat", "u1", vec![], true);
        let json = turn.to_ndjson().unwrap();
        assert!(json.contains("\"permission\":true"));
        assert!(json.contains("\"session_id\":\"s1\""));
        assert!(json.contains("\"session_name\":\"feat\""));
    }

    #[test]
    fn turn_object_with_attachment() {
        let att = Attachment {
            kind: "image".into(),
            url: "https://x/a.png".into(),
            filename: Some("a.png".into()),
            mime_type: None,
            size: None,
        };
        let turn = TurnObject::new("see", "", "default", "u1", vec![att], false);
        let json = turn.to_ndjson().unwrap();
        assert!(json.contains("\"kind\":\"image\""));
        assert!(json.contains("\"url\":\"https://x/a.png\""));
        assert!(json.contains("\"filename\":\"a.png\""));
        // None fields are skipped.
        assert!(!json.contains("mime_type"));
        assert!(!json.contains("\"size\""));
    }

    #[test]
    fn parse_partial_event() {
        let ev = parse_event(r#"{"type":"partial","text":"hello "}"#).unwrap();
        match ev {
            AgentEvent::Partial { text, role } => {
                assert_eq!(text, "hello ");
                assert_eq!(role, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_partial_with_thinking_role() {
        let ev = parse_event(r#"{"type":"partial","text":"hm","role":"thinking"}"#).unwrap();
        match ev {
            AgentEvent::Partial {
                role: Some(PartialRole::Thinking),
                ..
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_text_event() {
        let ev = parse_event(r#"{"type":"text","text":"final"}"#).unwrap();
        match ev {
            AgentEvent::Text { text } => assert_eq!(text, "final"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_session_event() {
        let ev = parse_event(r#"{"type":"session","id":"sess-1"}"#).unwrap();
        match ev {
            AgentEvent::Session { id } => assert_eq!(id, "sess-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_error_event() {
        let ev = parse_event(r#"{"type":"error","message":"boom"}"#).unwrap();
        match ev {
            AgentEvent::Error { message } => assert_eq!(message, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_permission_request_event() {
        let ev = parse_event(
            r#"{"type":"permission_request","request_id":"1","tool_name":"Bash","input":{"command":"echo hi"}}"#,
        )
        .unwrap();
        match ev {
            AgentEvent::PermissionRequest(req) => {
                assert_eq!(req.request_id, "1");
                assert_eq!(req.tool_name, "Bash");
                assert_eq!(req.input["command"], "echo hi");
                assert!(req.description.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        assert!(parse_event(r#"{"type":"tool_call","text":"x"}"#).is_none());
    }

    #[test]
    fn parse_malformed_json_returns_none() {
        assert!(parse_event("not json").is_none());
        assert!(parse_event("").is_none());
        assert!(parse_event(r#""just a string""#).is_none());
        assert!(parse_event("123").is_none());
    }

    #[test]
    fn parse_missing_type_returns_none() {
        assert!(parse_event(r#"{"text":"no type"}"#).is_none());
    }

    #[test]
    fn permission_response_allow_serializes() {
        let json = PermissionResponse::allow("42").to_ndjson().unwrap();
        assert!(json.contains("\"type\":\"permission_response\""));
        assert!(json.contains("\"behavior\":\"allow\""));
        assert!(json.contains("\"request_id\":\"42\""));
        assert!(!json.contains("updated_input"));
        assert!(!json.contains("message"));
    }

    #[test]
    fn permission_response_deny_with_reason() {
        let json = PermissionResponse::deny("42", "not allowed")
            .to_ndjson()
            .unwrap();
        assert!(json.contains("\"behavior\":\"deny\""));
        assert!(json.contains("\"message\":\"not allowed\""));
    }
}
