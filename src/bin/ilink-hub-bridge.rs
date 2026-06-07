//! Connect to iLink Hub and run a local CLI for each inbound text message.
//!
//! - **显式 Token**：`--token` / `WEIXIN_TOKEN`
//! - **扫码配对**：`--pair`（或首次无凭证且你希望用手机确认）
//! - **零交互（默认）**：不传 token、且凭证路径**不存在**时，进程自行调用 Hub 的通用 `POST /hub/register`，
//!   将虚拟 token 写入本地 JSON（与配对成功后的格式相同），Hub 侧不区分调用方类型。
//!   若凭证文件**已存在但损坏或 token 为空**，默认**不会**静默覆盖（避免误伤扫码配对）；需删文件、
//!   用 `--token` / `--pair`，或显式 **`--force-register`**。
//!
//! 若 Hub 配置了 `ILINK_ADMIN_TOKEN`，本进程注册时需在同一环境中设置该变量。
//!
//! **调试**：`ILINKHUB_BRIDGE_DUMP_MSG=1`（或 `true` / `yes`）时在 stderr 打印每条入站的完整 `WeixinMessage` JSON 与各 `item_list[*].extra`。
//!
//! 配置见 `docs/bridge/README.md`。

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use ilink_hub::bridge::{resolve_hub_connection, run_bridge, BridgeApp};

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

    /// Virtual token. Omit to use saved local credentials, auto-register, or `--pair` QR flow.
    #[arg(long, env = "WEIXIN_TOKEN")]
    token: Option<String>,

    /// Local credential JSON path (default: ~/.ilink-hub/bridge-credentials.json).
    #[arg(long, env = "ILINKHUB_BRIDGE_CREDS")]
    cred_file: Option<String>,

    /// Ignore saved credentials and run Hub QR pairing (phone confirm).
    #[arg(long, default_value_t = false)]
    pair: bool,

    /// Stable client name when auto-registering via `/hub/register` (optional; default random `local-<uuid>`).
    #[arg(long, env = "ILINKHUB_BRIDGE_REGISTER_NAME")]
    register_name: Option<String>,

    /// If the credential file exists but is invalid or has an empty token, delete it and auto-register again.
    #[arg(long, default_value_t = false)]
    force_register: bool,

    /// Path to bridge YAML (command, args, timeout, …).
    #[arg(long, default_value = "ilink-hub-bridge.yaml")]
    config: PathBuf,
}

fn explicit_token(cli: &Cli) -> Option<&str> {
    cli.token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
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

    let (hub_url, token) = resolve_hub_connection(
        &cli.hub_url,
        explicit_token(&cli),
        cli.cred_file.as_deref(),
        cli.pair,
        cli.register_name.as_deref(),
        cli.force_register,
    )
    .await?;
    info!(%hub_url, "using Hub base URL for downstream");

    let app = BridgeApp::load(&cli.config)?;
    info!(config_path = %cli.config.display(), "loaded bridge config");

    let handle = tokio::spawn(run_bridge(hub_url, token, app));
    let _ = tokio::signal::ctrl_c().await;
    handle.abort();
    let _ = handle.await;
    info!("exit");
    Ok(())
}
