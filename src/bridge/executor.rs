//! Shared bridge helpers retained after the agentproc-rs migration.
//!
//! The bulk of the old executor (run_cli, CliRunSummary, apply_placeholders,
//! permission helpers, env expansion, stream functions) moved to agentproc-rs
//! and `dispatcher/agentproc_runner`. This module keeps the three helpers that
//! are still referenced by the dispatcher and builtin profiles:
//!
//! - [`MAX_CLI_CAPTURE_BYTES`] — stdout/stderr safety cap (builtin/common,
//!   builtin/recursive).
//! - [`build_attachments`] — WeChat message → AgentProc attachments (dispatcher).
//! - [`split_into_parts`] — split a long reply into char-bounded parts
//!   (dispatcher).

use crate::bridge::protocol::Attachment;
use crate::ilink::types::WeixinMessage;

/// Hard upper bound on how many bytes of a child's stdout/stderr we buffer in
/// memory before truncating. A misbehaving CLI could otherwise stream unbounded
/// output and OOM the bridge. Safety valve only — the final reply is separately
/// truncated to `max_reply_chars` (default 8000).
pub const MAX_CLI_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// Build the `attachments` array for the turn object from a WeChat message's
/// media items. Under agentproc 0.4 all media travels in the turn object's
/// `attachments` field (no more `AGENT_IMAGE_URL` env vars).
pub(super) fn build_attachments(msg: &WeixinMessage) -> Vec<Attachment> {
    use crate::ilink::types::msg_type;
    let mut out = Vec::new();
    let Some(items) = msg.item_list.as_ref() else {
        return out;
    };
    for item in items.iter() {
        match item.item_type {
            Some(msg_type::IMAGE) => {
                if let Some(url) = item
                    .image_item
                    .as_ref()
                    .and_then(|i| i.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(Attachment {
                        kind: "image".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            Some(msg_type::FILE) => {
                if let Some(fi) = item.file_item.as_ref() {
                    if let Some(url) = fi.cdn_url.as_deref().filter(|s| !s.is_empty()) {
                        out.push(Attachment {
                            kind: "file".into(),
                            url: url.to_string(),
                            filename: fi.file_name.as_deref().map(|s| s.to_string()),
                            mime_type: None,
                            size: None,
                        });
                    }
                }
                break;
            }
            Some(msg_type::VIDEO) => {
                if let Some(url) = item
                    .video_item
                    .as_ref()
                    .and_then(|v| v.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(Attachment {
                        kind: "video".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            _ => {}
        }
    }
    out
}

/// Split a long reply body into parts each at most `max_chars` characters, so a
/// single huge reply becomes multiple WeChat messages instead of one truncated
/// blob. Returns at least one part (possibly empty).
pub(super) fn split_into_parts(s: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![s.to_string()];
    }
    let mut parts = Vec::new();
    let mut chars = s.chars().peekable();
    while chars.peek().is_some() {
        let part: String = chars.by_ref().take(max_chars).collect();
        parts.push(part);
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_into_parts_respects_limit() {
        assert_eq!(split_into_parts("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(split_into_parts("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(split_into_parts("hi", 10), vec!["hi"]);
        assert_eq!(split_into_parts("", 8), vec![""]);
    }

    #[test]
    fn split_into_parts_zero_limit_returns_whole() {
        assert_eq!(split_into_parts("abc", 0), vec!["abc"]);
    }

    #[test]
    fn split_into_parts_handles_multibyte() {
        assert_eq!(
            split_into_parts("一二三四五", 2),
            vec!["一二", "三四", "五"]
        );
    }
}
