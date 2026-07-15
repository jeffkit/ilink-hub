//! Smoke test: drive agentproc::run with executor: claude-code through the
//! ilink-hub worktree's dependency tree. Validates the in-process path that
//! the dispatcher now takes for `type: claude-code` profiles.
//!
//! Run from the worktree root:
//!   ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... \
//!     cargo run --example smoke_agentproc -- "reply with exactly: smoke ok"
//!
//! Expects CLAUDE_MODEL env (or defaults to glm-5.2). Reads ANTHROPIC_API_KEY
//! and ANTHROPIC_BASE_URL from the environment and forwards them to claude.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use agentproc::{run, Profile, RunOptions};

    let args: Vec<String> = std::env::args().collect();
    let message = if args.len() > 1 {
        args[1].as_str()
    } else {
        "reply with exactly: smoke ok"
    };

    let claude_model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "glm-5.2".to_string());
    let anthropic_key =
        std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY must be set")?;
    let anthropic_base =
        std::env::var("ANTHROPIC_BASE_URL").map_err(|_| "ANTHROPIC_BASE_URL must be set")?;

    // Build the profile programmatically (mirrors what dispatcher's
    // to_agentproc_profile produces for `type: claude-code`).
    let profile = Profile {
        executor: Some("claude-code".to_string()),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: [
            ("CLAUDE_MODEL".to_string(), claude_model),
            ("ANTHROPIC_API_KEY".to_string(), anthropic_key),
            ("ANTHROPIC_BASE_URL".to_string(), anthropic_base),
        ]
        .into_iter()
        .collect(),
        env_allowlist: Some(
            ["CLAUDE_MODEL", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        timeout_secs: 120,
        streaming: true,
        permission: false,
        ..Default::default()
    };

    let opts = RunOptions::new(message)
        .on_partial(|text, _| println!("[partial] {text}"))
        .on_session(|sid| eprintln!("[session] {sid}"));

    let result = run(&profile, opts).await?;

    eprintln!(
        "[result] exit={} session={} timed_out={} duration_ms={}",
        result.exit_code, result.session_id, result.timed_out, result.duration_ms
    );
    if !result.reply.is_empty() {
        println!("{}", result.reply);
    }
    if !result.error.is_empty() {
        eprintln!("[error] {}", result.error);
        std::process::exit(1);
    }
    Ok(())
}
