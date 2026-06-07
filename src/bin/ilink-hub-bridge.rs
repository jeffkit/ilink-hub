//! Connect to iLink Hub and run a local CLI for each inbound text message.
//!
//! - **Token**：`--token` / `WEIXIN_TOKEN`（与 `ilink-hub register` 相同），或
//! - **扫码配对**：不传 token 时，走 Hub 终端二维码（凭证默认 `~/.ilink-hub/bridge-credentials.json`，可用 `ILINKHUB_BRIDGE_CREDS` 覆盖）。
//!
//! 配置见 `docs/bridge/README.md`。

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use ilink_hub::bridge::{run_bridge, BridgeConfig};
use ilink_hub::client::{HubPairingClient, HubPairingOptions};

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

    /// Virtual token from `ilink-hub register` or pairing. Omit to load saved pairing creds or scan QR.
    #[arg(long, env = "WEIXIN_TOKEN")]
    token: Option<String>,

    /// JSON file for pairing credentials (default: ~/.ilink-hub/bridge-credentials.json).
    #[arg(long, env = "ILINKHUB_BRIDGE_CREDS")]
    cred_file: Option<String>,

    /// Ignore saved pairing file and run Hub QR pairing again.
    #[arg(long, default_value_t = false)]
    pair: bool,

    /// Path to bridge YAML (command, args, timeout, …).
    #[arg(long, default_value = "ilink-hub-bridge.yaml")]
    config: PathBuf,
}

fn default_bridge_cred_path() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ilink-hub")
        .join("bridge-credentials.json")
        .to_string_lossy()
        .into_owned()
}

fn explicit_token(cli: &Cli) -> Option<String> {
    cli.token
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

async fn resolve_hub_and_token(cli: &Cli) -> Result<(String, String)> {
    let hub = cli.hub_url.trim().trim_end_matches('/').to_string();

    if let Some(tok) = explicit_token(cli) {
        return Ok((hub, tok));
    }

    let cred_path = cli
        .cred_file
        .clone()
        .unwrap_or_else(default_bridge_cred_path);

    let mut opts = HubPairingOptions::new(&hub);
    opts.cred_path = Some(cred_path);
    opts.force = cli.pair;

    let client = HubPairingClient::new(opts);
    let creds = client.pair().await.context("Hub pairing")?;

    let base = creds.base_url.trim().trim_end_matches('/').to_string();
    Ok((base, creds.token))
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

    let (hub_url, token) = resolve_hub_and_token(&cli).await?;
    info!(%hub_url, "using Hub base URL for bridge");

    let config = BridgeConfig::load(&cli.config)?;
    info!(config_path = %cli.config.display(), "loaded bridge config");

    let handle = tokio::spawn(run_bridge(hub_url, token, config));
    let _ = tokio::signal::ctrl_c().await;
    handle.abort();
    let _ = handle.await;
    info!("exit");
    Ok(())
}
