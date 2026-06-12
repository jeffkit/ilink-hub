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
//! **内置 Profile**：`ilink-hub-bridge profile <type>` 运行内置 profile 处理器（如 `claude-code`），
//! 遵循 P0 exec 协议：从 `ILINK_*` 环境变量读取输入，向 stdout 写出回复。
//!
//! 配置见 `docs/bridge/README.md`，内置 profile 规范见 `docs/bridge/profile-spec.md`。

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use anyhow::Context;
use ilink_hub::bridge::{
    builtin, default_local_credential_path, resolve_hub_connection, run_bridge, BridgeApp,
    BridgeStop,
};
use ilink_hub::paths::{
    default_bridge_config_path, default_bridge_manager_credentials_dir, default_bridge_profiles_dir,
};

#[derive(Parser)]
#[command(name = "ilink-hub-bridge")]
#[command(
    version,
    about = "将微信（通过 iLink Hub）桥接到本地编码 CLI (Claude Code, Codex, …) / Bridge WeChat (via iLink Hub) to a local coding CLI (Claude Code, Codex, …)"
)]
struct Cli {
    /// Hub base URL (same as WEIXIN_BASE_URL for other backends).
    #[arg(
        long,
        env = "WEIXIN_BASE_URL",
        default_value_t = get_hub_url_default(),
        global = true
    )]
    hub_url: String,

    /// Virtual token. Omit to use saved local credentials, auto-register, or `--pair` QR flow.
    #[arg(long, env = "WEIXIN_TOKEN", global = true)]
    token: Option<String>,

    /// Local credential JSON path (default: ~/.ilink-hub/bridge-credentials.json).
    #[arg(long, env = "ILINKHUB_BRIDGE_CREDS", global = true)]
    cred_file: Option<String>,

    /// Ignore saved credentials and run Hub QR pairing (phone confirm).
    #[arg(long, default_value_t = false, global = true)]
    pair: bool,

    /// Stable client name when auto-registering via `/hub/register`.
    /// Default: `local-<hostname>-<config-stem>` (e.g. `local-MacBook-ilink-claude`).
    #[arg(long, env = "ILINKHUB_BRIDGE_REGISTER_NAME", global = true)]
    register_name: Option<String>,

    /// If the credential file exists but is invalid or has an empty token, delete it and auto-register again.
    #[arg(long, default_value_t = false, global = true)]
    force_register: bool,

    /// Path to bridge YAML (command, args, timeout, …). Used only in bridge (default) mode.
    /// Defaults to `~/.ilink-hub/ilink-hub-bridge.yaml`.
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a built-in profile handler (P0 exec protocol: reads ILINK_* env vars, writes to stdout).
    ///
    /// Example: ilink-hub-bridge profile claude-code
    ///
    /// Built-in types:
    ///   claude-code   Wrap the `claude` CLI with automatic --resume session continuity
    Profile {
        /// Built-in profile type (e.g. `claude-code`).
        #[arg(value_name = "TYPE")]
        profile_type: String,
    },
    /// Discover profile YAML files and supervise one bridge workspace per file.
    ///
    /// Each `*.yaml` / `*.yml` file keeps the existing bridge YAML format. The manager derives a
    /// stable workspace/register name from the file stem and stores a separate credential JSON per
    /// file, so every child bridge registers as an independent Hub backend.
    Manager {
        /// Directory containing bridge profile YAML files.
        #[arg(long, default_value_os_t = default_bridge_profiles_dir())]
        profiles_dir: PathBuf,

        /// Directory for per-profile bridge credential JSON files.
        #[arg(long, default_value_os_t = default_bridge_manager_credentials_dir())]
        credentials_dir: PathBuf,

        /// Seconds between profile directory scans.
        #[arg(long, default_value_t = 5)]
        scan_interval_secs: u64,

        /// Minimum seconds before restarting an exited child bridge.
        #[arg(long, default_value_t = 5)]
        restart_backoff_secs: u64,

        /// Maximum seconds for exponential child restart backoff.
        #[arg(long, default_value_t = 60)]
        max_restart_backoff_secs: u64,
    },
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

    match &cli.command {
        Some(Commands::Profile { profile_type }) => {
            // Run as a built-in profile subprocess (P0 exec protocol).
            // No Hub connection needed — just read env vars and write to stdout.
            builtin::run_builtin_profile(profile_type).await
        }
        Some(Commands::Manager {
            profiles_dir,
            credentials_dir,
            scan_interval_secs,
            restart_backoff_secs,
            max_restart_backoff_secs,
        }) => {
            if explicit_token(&cli).is_some()
                || cli.cred_file.is_some()
                || cli.register_name.is_some()
                || cli.pair
            {
                tracing::warn!(
                    "manager mode ignores --token/WEIXIN_TOKEN, --cred-file, --register-name, and --pair; \
                     each profile gets an independent auto-registered child bridge"
                );
            }
            // Child bridges inherit this process's environment, so a manager-level
            // ILINK_ADMIN_TOKEN propagates to every child's `/hub/register` call. If it is
            // missing and the Hub enforces admin auth, auto-registration fails with 401 and
            // operators are tempted to hand-craft credentials that reuse another backend's
            // vtoken — which makes multiple bridges share one message queue (split-brain).
            if std::env::var("ILINK_ADMIN_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .is_none()
            {
                tracing::warn!(
                    "ILINK_ADMIN_TOKEN is not set for the bridge manager. If the Hub enforces \
                     admin auth, child bridges will fail to auto-register (HTTP 401). Set \
                     ILINK_ADMIN_TOKEN (matching the Hub) in the manager's environment so each \
                     profile registers as an independent backend. Never reuse another backend's \
                     credentials/token to work around this — sharing a vtoken makes bridges \
                     compete for the same message queue."
                );
            }
            let mut opts = ilink_hub::bridge::manager::BridgeManagerOptions::new(
                cli.hub_url.clone(),
                profiles_dir.clone(),
                credentials_dir.clone(),
            );
            opts.scan_interval = std::time::Duration::from_secs((*scan_interval_secs).max(1));
            opts.restart_backoff = std::time::Duration::from_secs((*restart_backoff_secs).max(1));
            opts.max_restart_backoff =
                std::time::Duration::from_secs((*max_restart_backoff_secs).max(1));
            opts.force_register = cli.force_register;
            ilink_hub::bridge::manager::run_bridge_manager(opts).await
        }
        None => {
            // Default mode: connect to Hub and long-poll for messages.
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(default_bridge_config_path);
            let app = BridgeApp::load(&config_path)?;
            info!(config_path = %config_path.display(), "loaded bridge config");

            let cred_path = cli
                .cred_file
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(default_local_credential_path);
            let using_explicit_token = explicit_token(&cli).is_some();

            'reconnect: loop {
                let (hub_url, token) = resolve_hub_connection(
                    &cli.hub_url,
                    explicit_token(&cli),
                    cli.cred_file.as_deref(),
                    cli.pair,
                    cli.register_name.as_deref(),
                    cli.force_register,
                    Some(config_path.as_path()),
                )
                .await?;
                info!(%hub_url, "using Hub base URL for downstream");

                let mut handle = tokio::spawn(run_bridge(hub_url, token, app.clone()));
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        handle.abort();
                        let _ = handle.await;
                        info!("exit");
                        return Ok(());
                    }
                    result = &mut handle => {
                        match result {
                            Ok(BridgeStop::TokenRejected) if using_explicit_token => {
                                anyhow::bail!(
                                    "Hub 拒绝了 WEIXIN_TOKEN / --token（未注册或已失效）。\
                                     请重新执行 `ilink-hub register` 或 `ilink-hub-bridge --force-register`。"
                                );
                            }
                            Ok(BridgeStop::TokenRejected) => {
                                warn!(
                                    path = %cred_path.display(),
                                    "hub token revoked at runtime; removing credentials and re-registering"
                                );
                                let _ = tokio::fs::remove_file(&cred_path).await;
                                continue 'reconnect;
                            }
                            Err(e) => {
                                return Err(e).context("bridge task panicked or failed");
                            }
                        }
                    }
                }
            }
        }
    }
}

fn get_hub_url_default() -> String {
    if let Ok(val) = std::env::var("WEIXIN_BASE_URL") {
        if !val.trim().is_empty() {
            return val.trim().to_string();
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_URL") {
        if !val.trim().is_empty() {
            return val.trim().to_string();
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_ADDR") {
        if !val.trim().is_empty() {
            let val_trimmed = val.trim();
            if val_trimmed.starts_with("http://") || val_trimmed.starts_with("https://") {
                return val_trimmed.to_string();
            } else {
                return format!("http://{}", val_trimmed);
            }
        }
    }
    "http://127.0.0.1:8765".to_string()
}
