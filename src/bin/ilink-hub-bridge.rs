//! Connect to iLink Hub with a virtual token and run a local CLI for each inbound text message.
//!
//! Configure `WEIXIN_BASE_URL` / `WEIXIN_TOKEN` (or `--hub-url` / `--token`) and a YAML file.
//! See `docs/bridge/README.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use ilink_hub::bridge::{run_bridge, BridgeConfig};

#[derive(Parser)]
#[command(name = "ilink-hub-bridge")]
#[command(
    version,
    about = "Bridge WeChat (via iLink Hub) to a local coding CLI (Claude Code, Codex, …)"
)]
struct Cli {
    /// Hub base URL (same as WEIXIN_BASE_URL for other backends).
    #[arg(long, env = "WEIXIN_BASE_URL", default_value = "http://127.0.0.1:8765")]
    hub_url: String,

    /// Virtual token from `ilink-hub register` (same as WEIXIN_TOKEN).
    #[arg(long, env = "WEIXIN_TOKEN")]
    token: String,

    /// Path to bridge YAML (command, args, timeout, …).
    #[arg(long, default_value = "ilink-hub-bridge.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ilink_hub=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    if cli.token.trim().is_empty() {
        anyhow::bail!("missing token: set --token or WEIXIN_TOKEN (from `ilink-hub register`)");
    }

    let config = BridgeConfig::load(&cli.config)?;
    info!(config_path = %cli.config.display(), "loaded bridge config");

    let handle = tokio::spawn(run_bridge(cli.hub_url, cli.token, config));
    let _ = tokio::signal::ctrl_c().await;
    handle.abort();
    let _ = handle.await;
    info!("exit");
    Ok(())
}
