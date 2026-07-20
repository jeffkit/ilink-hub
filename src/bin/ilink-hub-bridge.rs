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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use std::io::IsTerminal;

use anyhow::Context;
use ilink_hub::bridge::transport::{IlinkTransport, NullTransport, Transport};
use ilink_hub::bridge::{
    builtin, default_direct_credential_path, default_local_credential_path,
    resolve_direct_connection, resolve_hub_connection, run_bridge_with_shutdown, BridgeApp,
    BridgeStop, Via,
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

    /// Allow a non-`ilink` `transport:` to load its placeholder adapter. Without
    /// this flag a non-ilink transport fails fast at startup (it would otherwise
    /// back off forever as a zombie). Intended for pluggability smoke-tests only.
    #[arg(
        long,
        default_value_t = false,
        env = "ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT",
        global = true
    )]
    allow_null_transport: bool,

    /// Disable interactive flows (QR login prompts). When set (or when stdout
    /// is not a TTY), `via: direct` bails instead of printing a QR code — a
    /// headless supervisor cannot confirm a phone scan. The bridge manager
    /// injects this env into its children so they fail fast and let the
    /// manager's credential guard park the profile.
    #[arg(
        long,
        default_value_t = false,
        env = "ILINKHUB_BRIDGE_NON_INTERACTIVE",
        global = true
    )]
    no_interactive: bool,

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

/// The localhost Hub URL used as the CLI default when `WEIXIN_BASE_URL` is unset.
/// `via: direct` refusing to fall back to this value prevents silently pointing
/// a direct bridge at a Hub/localhost (review M2).
const DEFAULT_HUB_URL: &str = "http://127.0.0.1:8765";

/// Resolve the iLink upstream base URL for a `via: direct` profile (review M2).
///
/// - A YAML `base_url:` always wins (lets a manager mix hub/direct profiles
///   against different upstreams).
/// - Otherwise the CLI/env `--hub-url` / `WEIXIN_BASE_URL` is used — but the
///   localhost Hub default is rejected, so a direct bridge never silently
///   targets a Hub/localhost and fires `get_bot_qrcode` at the wrong server.
fn resolve_direct_base_url(direct_base_url: Option<&str>, cli_hub_url: &str) -> Result<String> {
    if let Some(b) = direct_base_url {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "via: direct profile has empty `base_url:`; set it to the real iLink upstream"
            );
        }
        return Ok(trimmed.trim_end_matches('/').to_string());
    }
    let cli_base = cli_hub_url.trim().trim_end_matches('/').to_string();
    if cli_base == DEFAULT_HUB_URL {
        anyhow::bail!(
            "via: direct 需要显式 `base_url:`（YAML）或非默认 `WEIXIN_BASE_URL` 指向真实 iLink 上游。\
             当前 base 仍是默认 localhost Hub 地址 ({DEFAULT_HUB_URL})，直接对它发 get_bot_qrcode \
             语义错误。若确实要连本机 Hub，请改用 `via: hub`。"
        );
    }
    Ok(cli_base)
}

/// Build the configured transport for the bridge run.
///
/// - `transport: ilink` + `via: hub` (default): resolve a virtual token via the Hub
///   (`/hub/register` / QR) and point `IlinkTransport` at the Hub.
/// - `transport: ilink` + `via: direct`: connect straight to the real iLink
///   upstream. Stage 3 resolves credentials via: explicit `WEIXIN_TOKEN` →
///   `--pair` QR login against the real upstream → saved direct cred file.
///   The upstream base URL is `base_url:` from the YAML when set, else a
///   non-default `--hub-url` / `WEIXIN_BASE_URL` (a localhost/default URL is
///   rejected to avoid silently targeting a Hub — review M2).
/// - `transport: <other>`: load a `NullTransport` placeholder only when
///   `--allow-null-transport` is set; otherwise fail fast (the placeholder would
///   otherwise back off forever as a zombie — review L4).
async fn build_transport(
    app: &BridgeApp,
    cli: &Cli,
    config_path: &Path,
    description: Option<&str>,
    interactive: bool,
) -> Result<Arc<dyn Transport>> {
    let transport = app.transport();
    if !transport.is_ilink() {
        let name = transport.as_str().to_string();
        if !cli.allow_null_transport {
            anyhow::bail!(
                "transport `{name}` 没有真实适配器（占位 NullTransport 会永久退避成僵尸进程）。\
                 如仅为可插拔冒烟测试，请加 `--allow-null-transport` 显式开启占位。"
            );
        }
        info!(transport = %name, "loading placeholder transport (allow_null_transport)");
        return Ok(Arc::new(NullTransport::new(name)));
    }

    let t: Arc<dyn Transport> = match app.via() {
        Via::Hub => {
            let (hub_url, token) = resolve_hub_connection(
                &cli.hub_url,
                explicit_token(cli),
                cli.cred_file.as_deref(),
                cli.pair,
                cli.register_name.as_deref(),
                cli.force_register,
                Some(config_path),
                description,
            )
            .await?;
            info!(%hub_url, via = "hub", "using Hub base URL for downstream");
            Arc::new(IlinkTransport::new(hub_url, token).context("build iLink transport")?)
        }
        Via::Direct => {
            // YAML `base_url:` overrides the CLI/env URL for this profile, so a
            // bridge manager can mix hub and direct profiles against different
            // upstreams. Without `base_url:`, require a non-default `--hub-url` /
            // `WEIXIN_BASE_URL` — refusing the localhost Hub default avoids
            // silently pointing a direct bridge at a Hub (review M2).
            let base = resolve_direct_base_url(app.direct_base_url(), &cli.hub_url)?;
            let (base, token) = resolve_direct_connection(
                &base,
                explicit_token(cli),
                cli.cred_file.as_deref(),
                cli.pair,
                cli.force_register,
                Some(config_path),
                interactive,
            )
            .await?;
            info!(base = %base, via = "direct", "connecting directly to iLink upstream");
            // direct mode cannot resume CLI sessions across messages (the real
            // upstream does not echo the HubExt session_id the Hub persists).
            info!(
                "via: direct 不支持跨消息 CLI 会话续接（真实上游不回显 session_id）；每条消息起新 CLI 会话。"
            );
            let t = IlinkTransport::new(base, token).context("build iLink transport (direct)")?;
            Arc::new(t)
        }
    };
    // N4: log capabilities at the common exit so both hub and direct paths are
    // observable (review M6). media_upload is not implemented for iLink today.
    let caps = t.capabilities();
    info!(media_upload = caps.media_upload, "transport capabilities");
    Ok(t)
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
                .unwrap_or_else(|| match app.via() {
                    Via::Direct => default_direct_credential_path(),
                    Via::Hub => default_local_credential_path(),
                });
            let using_explicit_token = explicit_token(&cli).is_some();

            // Interactive flows (QR login) require a TTY for stdout and must not
            // be disabled via --no-interactive / ILINKHUB_BRIDGE_NON_INTERACTIVE
            // (the manager injects the latter so its children fail fast instead
            // of QR-blocking headless — review N1).
            let interactive = !cli.no_interactive && std::io::stdout().is_terminal();

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

                let transport =
                    build_transport(&app, &cli, &config_path, description, interactive).await?;

                let mut handle = tokio::spawn(run_bridge_with_shutdown(
                    transport,
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
                                let via = app.via();
                                let hint = if via.is_direct() {
                    "via: direct 下请重新 `--pair` 扫码登录真实上游，或更换为有效的 WEIXIN_TOKEN。"
                                } else {
                    "via: hub 下请重新执行 `ilink-hub register` 或 `ilink-hub-bridge --force-register`。"
                                };
                                anyhow::bail!(
                                    "WEIXIN_TOKEN / --token 被拒绝（未注册或已失效）。{hint}"
                                );
                            }
                            Ok(BridgeStop::TokenRejected) => {
                                let via = app.via();
                                let what = if via.is_direct() {
                                    "direct token"
                                } else {
                                    "hub token"
                                };
                                warn!(
                                    path = %cred_path.display(),
                                    "{what} revoked at runtime; removing credentials and reconnecting"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_yaml(dir: &Path, content: &str) -> PathBuf {
        let p = dir.join("profile.yaml");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn resolve_direct_base_url_rejects_default_hub_url_without_base_url() {
        // M2: via: direct, no `base_url:`, CLI hub-url at the localhost default → bail.
        let err = resolve_direct_base_url(None, "http://127.0.0.1:8765").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct") && msg.contains("base_url"),
            "expected M2 bail mentioning via: direct + base_url: {msg}"
        );
    }

    #[test]
    fn resolve_direct_base_url_accepts_non_default_cli_url() {
        // A non-default --hub-url / WEIXIN_BASE_URL is acceptable for direct.
        let base = resolve_direct_base_url(None, "https://ilinkai.weixin.qq.com/").unwrap();
        assert_eq!(base, "https://ilinkai.weixin.qq.com");
    }

    #[test]
    fn resolve_direct_base_url_yaml_overrides_cli() {
        // `base_url:` in YAML wins even if the CLI url is the localhost default.
        let base = resolve_direct_base_url(
            Some("https://ilinkai.weixin.qq.com"),
            "http://127.0.0.1:8765",
        )
        .unwrap();
        assert_eq!(base, "https://ilinkai.weixin.qq.com");
    }

    #[test]
    fn resolve_direct_base_url_rejects_empty_yaml_base_url() {
        let err = resolve_direct_base_url(Some("   "), "https://up.example.com").unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn no_interactive_bare_flag_parses_true() {
        // N1 regression guard: `--no-interactive` must be a bare SetTrue flag
        // (the manager passes it bare to children). The env form only accepts
        // "true"/"false", so the manager must NOT inject "1" via env.
        let cli = Cli::parse_from([
            "ilink-hub-bridge",
            "--no-interactive",
            "--hub-url",
            "http://x",
        ]);
        assert!(cli.no_interactive);
        let cli = Cli::parse_from(["ilink-hub-bridge", "--hub-url", "http://x"]);
        assert!(!cli.no_interactive);
    }

    #[test]
    fn build_transport_direct_bails_without_base_url() {
        // End-to-end-ish: build_transport on a via: direct profile with no base_url
        // and the default hub-url bails at the M2 gate before any network call.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\nvia: direct\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["ilink-hub-bridge", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected M2 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct") && msg.contains("base_url"),
            "expected M2 bail: {msg}"
        );
    }

    #[test]
    fn build_transport_non_ilink_transport_bails_without_allow_flag() {
        // L4: a non-ilink transport fails fast unless --allow-null-transport is set.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: wecom\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["ilink-hub-bridge", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected L4 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("wecom") && msg.contains("--allow-null-transport"),
            "expected L4 bail mentioning transport + flag: {msg}"
        );
    }
}
