//! Pure AgentProc 0.4 stdout assembly — shared by the bridge executor semantics
//! tests and (optionally) conformance fixtures from upstream `scenarios.json`.
//!
//! This module intentionally mirrors the official agentproc runner's observable
//! outputs (`reply`, `session_id`, `error`, `partials`, `usage`) without the
//! WeChat-specific side effects (e.g. forwarding `error.message` as a live
//! partial). The live executor may still choose a different UX for errors.

use crate::bridge::protocol::{self, AgentEvent};

/// Config knobs that affect assembly (profile / test overrides).
#[derive(Debug, Clone)]
pub struct WireAssembleConfig {
    pub streaming: bool,
    pub max_reply_chars: usize,
    pub truncation_suffix: String,
}

impl Default for WireAssembleConfig {
    fn default() -> Self {
        Self {
            streaming: true,
            max_reply_chars: 8000,
            // Match agentproc runner default (EN). Product profiles may override
            // with a Chinese suffix via YAML.
            truncation_suffix: "\n\n…(truncated)".into(),
        }
    }
}

/// Observable outcome of one agent turn's stdout (wire 0.4).
#[derive(Debug, Clone, PartialEq)]
pub struct WireAssembleOutcome {
    /// Final non-streaming body (empty when streaming partials already carried it).
    pub reply: String,
    pub session_id: Option<String>,
    pub error: Option<String>,
    pub partials: Vec<String>,
    pub usage: Option<serde_json::Value>,
    /// 1 when an `error` event was seen, else 0 (matches agentproc scenarios).
    pub exit_code: i32,
}

/// Whether a session id is safe to persist (agentproc runner parity).
///
/// Rejects empty, `.` / `..`, path separators, and ASCII control characters.
pub fn is_valid_session_id(value: &str) -> bool {
    if value.is_empty() || value == "." || value == ".." {
        return false;
    }
    !value
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// Assemble one turn from raw stdout lines (already split, no trailing `\n` required).
pub fn assemble_lines(lines: &[impl AsRef<str>], cfg: &WireAssembleConfig) -> WireAssembleOutcome {
    let mut session_id: Option<String> = None;
    let mut result_text: Option<String> = None;
    let mut error: Option<String> = None;
    let mut partials: Vec<String> = Vec::new();
    let mut usage: Option<serde_json::Value> = None;
    let mut cumulative_partial_chars: usize = 0;
    let mut partials_truncated = false;
    let mut error_seen = false;

    for raw in lines {
        let line = raw.as_ref().trim_end_matches('\r');
        let Some(event) = protocol::parse_event(line) else {
            continue;
        };

        if let Some(sid) = event.session_id() {
            if is_valid_session_id(sid) {
                match &session_id {
                    None => session_id = Some(sid.to_string()),
                    Some(existing) if existing != sid => {
                        // keep first — protocol violation is fail-soft
                    }
                    Some(_) => {}
                }
            }
        }

        match event {
            AgentEvent::Partial { text, .. } => {
                if error_seen || !cfg.streaming || partials_truncated || text.is_empty() {
                    continue;
                }
                let remaining = cfg.max_reply_chars.saturating_sub(cumulative_partial_chars);
                if remaining == 0 {
                    partials.push(cfg.truncation_suffix.clone());
                    partials_truncated = true;
                    continue;
                }
                if text.chars().count() > remaining {
                    // Implementation-defined boundary: we forward a tail-truncated slice.
                    let chunk: String = text.chars().take(remaining).collect();
                    if !chunk.is_empty() {
                        partials.push(chunk);
                    }
                    partials.push(cfg.truncation_suffix.clone());
                    partials_truncated = true;
                    cumulative_partial_chars = cfg.max_reply_chars;
                } else {
                    cumulative_partial_chars += text.chars().count();
                    partials.push(text);
                }
            }
            AgentEvent::Result { text, usage: u, .. } => {
                if error_seen {
                    continue;
                }
                if result_text.is_some() {
                    continue; // at most one
                }
                result_text = Some(text);
                if usage.is_none() {
                    usage = u;
                }
            }
            AgentEvent::Error {
                message, usage: u, ..
            } => {
                if !message.trim().is_empty() {
                    error = Some(message);
                } else if error.is_none() {
                    error = Some(String::new());
                }
                error_seen = true;
                if usage.is_none() {
                    usage = u;
                }
            }
            AgentEvent::PermissionRequest(_) => {
                // Not part of assemble observables for scenarios.json.
            }
        }
    }

    let exit_code = if error_seen { 1 } else { 0 };

    let reply = if error_seen {
        String::new()
    } else if cfg.streaming && !partials.is_empty() {
        // Partials already carried the user-visible body.
        String::new()
    } else {
        let mut body = result_text.unwrap_or_default();
        if !body.is_empty() && body.chars().count() > cfg.max_reply_chars {
            let truncated: String = body.chars().take(cfg.max_reply_chars).collect();
            body = format!("{truncated}{}", cfg.truncation_suffix);
        }
        body
    };

    WireAssembleOutcome {
        reply,
        session_id,
        error,
        partials,
        usage,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_separator_session_ids() {
        assert!(!is_valid_session_id("bad/path"));
        assert!(!is_valid_session_id(".."));
        assert!(is_valid_session_id("ok: spaced"));
        assert!(is_valid_session_id("valid-1"));
    }

    #[test]
    fn first_session_wins_and_second_result_ignored() {
        let lines = [
            r#"{"type":"result","text":"done","session_id":"first"}"#,
            r#"{"type":"result","text":"ignored","session_id":"second"}"#,
        ];
        let out = assemble_lines(&lines, &WireAssembleConfig::default());
        assert_eq!(out.reply, "done");
        assert_eq!(out.session_id.as_deref(), Some("first"));
        assert_eq!(out.exit_code, 0);
    }
}
