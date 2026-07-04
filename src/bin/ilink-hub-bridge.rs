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
//! 遵循 P0 exec 协议：从 `AGENT_*` 环境变量读取输入，向 stdout 写出回复。
//!
//! 配置见 `docs/bridge/README.md`，内置 profile 规范见 `docs/bridge/profile-spec.md`。

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use anyhow::Context;
use ilink_hub::bridge::{
    builtin, default_local_credential_path, resolve_hub_connection, run_bridge_with_shutdown,
    BridgeApp, BridgeStop,
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
    /// Run a built-in profile handler (P0 exec protocol: reads AGENT_* env vars, writes to stdout).
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

/// Resolves on SIGTERM on Unix; never resolves on other platforms.
/// Lets us use SIGTERM in `tokio::select!` without `#[cfg]` inside the macro.
async fn make_sigterm_future() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await;
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

    let has_deprecated_addr = std::env::var("ILINK_HUB_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    let has_deprecated_url = std::env::var("ILINK_HUB_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    let has_new_url = std::env::var("WEIXIN_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    if (has_deprecated_addr || has_deprecated_url) && !has_new_url {
        tracing::warn!(
            "The environment variables `ILINK_HUB_ADDR` and `ILINK_HUB_URL` are deprecated. \
             Please migrate to `WEIXIN_BASE_URL`."
        );
    }

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

            // Startup probe to verify that the CLI command(s) exist and are usable.
            for name in app.profile_names() {
                if let Some(profile) = app.profile(name) {
                    if let Err(e) = ilink_hub::bridge::probe_profile_light(profile) {
                        eprintln!("Startup probe failed for profile `{}`: {}", name, e);
                        std::process::exit(1);
                    }
                }
            }

            let cred_path = cli
                .cred_file
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(default_local_credential_path);
            let using_explicit_token = explicit_token(&cli).is_some();

            // Shared shutdown token — cancelled by Ctrl-C or SIGTERM so that
            // in-flight AI calls are gracefully cancelled and users are notified.
            let shutdown = tokio_util::sync::CancellationToken::new();

            // Build a SIGTERM future once, outside the reconnect loop.
            // On non-Unix platforms this never resolves (pending forever).
            let sigterm_fut = make_sigterm_future();
            tokio::pin!(sigterm_fut);

            'reconnect: loop {
                // Get description from default profile for registration
                let description = app
                    .profile(app.default_profile_name())
                    .and_then(|p| p.description.as_deref());

                let (hub_url, token) = resolve_hub_connection(
                    &cli.hub_url,
                    explicit_token(&cli),
                    cli.cred_file.as_deref(),
                    cli.pair,
                    cli.register_name.as_deref(),
                    cli.force_register,
                    Some(config_path.as_path()),
                    description,
                )
                .await?;
                info!(%hub_url, "using Hub base URL for downstream");

                let mut handle = tokio::spawn(run_bridge_with_shutdown(
                    hub_url,
                    token,
                    app.clone(),
                    shutdown.clone(),
                ));

                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("bridge received Ctrl-C; shutting down gracefully");
                        shutdown.cancel();
                        // Wait up to 3 s for error replies to be sent before aborting.
                        // Only abort+await when the task did NOT finish within the timeout;
                        // re-awaiting an already-completed JoinHandle causes a panic.
                        let timed_out = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            &mut handle,
                        ).await.is_err();
                        if timed_out {
                            handle.abort();
                            let _ = handle.await;
                        }
                        info!("exit");
                        return Ok(());
                    }
                    _ = &mut sigterm_fut => {
                        info!("bridge received SIGTERM; shutting down gracefully");
                        shutdown.cancel();
                        let timed_out = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            &mut handle,
                        ).await.is_err();
                        if timed_out {
                            handle.abort();
                            let _ = handle.await;
                        }
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
                            Ok(BridgeStop::FatalCliError(reason)) => {
                                anyhow::bail!(
                                    "CLI 认证失败，需要用户处理后重启 bridge：{reason}"
                                );
                            }
                            Ok(BridgeStop::Shutdown) => {
                                info!("bridge shut down gracefully");
                                return Ok(());
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
