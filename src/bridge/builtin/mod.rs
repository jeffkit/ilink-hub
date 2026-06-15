//! Built-in profile handlers for `ilink-hub-bridge profile <type>`.
//!
//! Each handler reads the P0 env vars injected by the bridge and writes to stdout:
//!   - Optional first line: `ILINK_SESSION:<uuid>`
//!   - Remaining lines: reply text for the WeChat user
//!
//! All built-ins follow the same P0 exec protocol as external scripts/SDKs.

mod agy;
mod claude_code;
mod codex;
mod cursor;

/// Dispatch to a built-in profile handler by type name.
///
/// Called from `ilink-hub-bridge profile <type>`.
pub async fn run_builtin_profile(profile_type: &str) -> anyhow::Result<()> {
    match profile_type {
        "claude-code" => claude_code::run().await,
        "cursor" => cursor::run().await,
        "codex" => codex::run().await,
        "agy" => agy::run().await,
        other => anyhow::bail!(
            "unknown built-in profile type `{other}`; supported: claude-code, cursor, codex, agy"
        ),
    }
}
