//! Shared bridge helpers retained after the agentproc-rs migration.
//!
//! The bulk of the old executor (run_cli, CliRunSummary, apply_placeholders,
//! permission helpers, env expansion, stream functions) moved to agentproc-rs
//! and `dispatcher/agentproc_runner`. This module keeps the helpers that are
//! still referenced by the dispatcher and builtin profiles:
//!
//! - [`MAX_CLI_CAPTURE_BYTES`] — stdout/stderr safety cap (builtin/common,
//!   builtin/recursive).
//! - [`split_into_parts`] — split a long reply into char-bounded parts
//!   (dispatcher).
//!
//! Media extraction now lives in `transport::ilink::build_media`, which emits
//! generic [`crate::bridge::transport::MediaRef`]s instead of iLink wire types.

/// Hard upper bound on how many bytes of a child's stdout/stderr we buffer in
/// memory before truncating. A misbehaving CLI could otherwise stream unbounded
/// output and OOM the bridge. Safety valve only — the final reply is separately
/// truncated to `max_reply_chars` (default 8000).
pub const MAX_CLI_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

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
