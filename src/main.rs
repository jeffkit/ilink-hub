use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::info;

use ilink_hub::{
    ilink::LoginClient, paths::default_database_url, run_serve, store::Store, ServeOptions,
};

#[derive(Parser)]
#[command(name = "ilink-hub")]
#[command(
    version,
    about = "微信 ClawBot 的 iLink 兼容多路复用 Hub / iLink-compatible multiplexer hub for WeChat ClawBot"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Hub 服务器 / Start the hub server
    Serve {
        /// Real iLink bot token. If omitted, loaded from DATABASE_URL or triggers QR login.
        #[arg(long, env = "ILINK_TOKEN")]
        token: Option<String>,

        /// Hub listen address
        #[arg(long, default_value_t = get_addr_default(), env = "WEIXIN_BASE_URL")]
        addr: String,

        /// Override real iLink base URL (for testing / custom deployments)
        #[arg(long, env = "ILINK_BASE_URL")]
        ilink_base_url: Option<String>,

        /// Database connection URL.
        /// Defaults to SQLite at ~/.ilink-hub/ilink-hub.db
        /// Examples:
        ///   sqlite:///path/to/db.sqlite
        ///   postgres://user:pass@localhost/ilink_hub
        ///   mysql://user:pass@localhost/ilink_hub
        #[arg(long, default_value_t = default_database_url(), env = "DATABASE_URL")]
        database_url: String,
    },

    /// Interactive QR-code login — scans WeChat and saves bot_token to DB
    Login {
        #[arg(long, default_value_t = default_database_url(), env = "DATABASE_URL")]
        database_url: String,

        #[arg(long, env = "ILINK_BASE_URL")]
        ilink_base_url: Option<String>,
    },

    /// 向 Hub 注册客户端（输出可用的 vtoken） / Register a backend client with the hub (outputs vtoken to use)
    Register {
        /// Hub URL
        #[arg(long, default_value_t = get_hub_url_default(), env = "WEIXIN_BASE_URL")]
        hub_url: String,

        /// Workspace name (short, machine-readable)
        #[arg(long)]
        name: String,

        /// Human-readable label
        #[arg(long)]
        label: Option<String>,
    },

    /// List registered clients
    Clients {
        #[arg(long, default_value_t = get_hub_url_default(), env = "WEIXIN_BASE_URL")]
        hub_url: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install sqlx drivers once at startup so every `Store::connect` call
    // doesn't pay the (admittedly small) `Once::call_once` atomic check.
    sqlx::any::install_default_drivers();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ilink_hub=debug".parse()?)
                .add_directive("tower_http=info".parse()?),
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

    match cli.command {
        Commands::Serve {
            token,
            addr,
            ilink_base_url,
            database_url,
        } => {
            let addr = extract_host_port(&addr).unwrap_or(addr);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let shutdown_tx_clone = shutdown_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = shutdown_on_signal().await {
                    tracing::warn!(error = %e, "shutdown signal handler failed; proceeding with shutdown");
                }
                info!("Received shutdown signal, shutting down");
                let _ = shutdown_tx_clone.send(true);
            });
            drop(shutdown_tx);
            run_serve(
                ServeOptions {
                    token,
                    addr,
                    ilink_base_url,
                    database_url,
                    on_listening: None,
                    qr_login_ui: None,
                    on_hub_state: None,
                },
                shutdown_rx,
            )
            .await?;
        }
        Commands::Login {
            database_url,
            ilink_base_url,
        } => {
            run_login(database_url, ilink_base_url).await?;
        }
        Commands::Register {
            hub_url,
            name,
            label,
        } => {
            register_client(&hub_url, name, label).await?;
        }
        Commands::Clients { hub_url } => {
            list_clients(&hub_url).await?;
        }
    }

    Ok(())
}

async fn run_login(database_url: String, ilink_base_url: Option<String>) -> Result<()> {
    let store = Store::connect(&database_url).await?;
    let login_client = LoginClient::new(ilink_base_url.clone());
    let token = login_client.login_with_qr().await?;
    let base = ilink_base_url.unwrap_or_else(|| "https://ilinkai.weixin.qq.com".to_string());
    store.save_credentials(&token, &base).await?;
    println!("✅ Login successful! Token saved to {}", database_url);
    Ok(())
}

async fn register_client(hub_url: &str, name: String, label: Option<String>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{hub_url}/hub/register"));
    if let Ok(token) = std::env::var("ILINK_ADMIN_TOKEN") {
        if !token.trim().is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token.trim()));
        }
    }
    let resp: serde_json::Value = req
        .json(&serde_json::json!({ "name": name, "label": label }))
        .send()
        .await?
        .json()
        .await?;

    if resp["ret"] == 0 {
        let vtoken = resp["vtoken"].as_str().unwrap_or("");
        println!("✅ Registered client '{name}'");
        println!();
        println!("Configure your backend with:");
        println!("  WEIXIN_BASE_URL={hub_url}");
        println!("  WEIXIN_TOKEN={vtoken}");
        println!();
        println!("Or for wechatbot Rust SDK:");
        println!("  BotOptions {{ base_url: Some(\"{hub_url}\"), token: \"{vtoken}\", .. }}");
    } else {
        eprintln!(
            "❌ Registration failed: {}",
            resp["errmsg"].as_str().unwrap_or("unknown")
        );
        std::process::exit(1);
    }

    Ok(())
}

async fn list_clients(hub_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{hub_url}/hub/clients"));
    if let Ok(token) = std::env::var("ILINK_ADMIN_TOKEN") {
        if !token.trim().is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token.trim()));
        }
    }
    let resp: serde_json::Value = req.send().await?.json().await?;

    let clients = resp["clients"].as_array().cloned().unwrap_or_default();
    if clients.is_empty() {
        println!("No clients registered.");
    } else {
        println!("{:<20} {:<12} LABEL", "NAME", "STATUS");
        println!("{}", "-".repeat(52));
        for c in clients {
            let name = c["name"].as_str().unwrap_or("-");
            let label = c["label"].as_str().unwrap_or("-");
            let online = if c["online"].as_bool().unwrap_or(false) {
                "🟢 online "
            } else {
                "🔴 offline"
            };
            println!("{:<20} {:<12} {}", name, online, label);
        }
    }

    Ok(())
}

fn extract_host_port(s: &str) -> Option<String> {
    ilink_hub::paths::parse_host_port(s)
}

fn get_addr_default() -> String {
    if let Ok(val) = std::env::var("WEIXIN_BASE_URL") {
        if !val.trim().is_empty() {
            if let Some(addr) = extract_host_port(&val) {
                return addr;
            }
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_ADDR") {
        if !val.trim().is_empty() {
            if let Some(addr) = extract_host_port(&val) {
                return addr;
            } else {
                return val.trim().to_string();
            }
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_URL") {
        if !val.trim().is_empty() {
            if let Some(addr) = extract_host_port(&val) {
                return addr;
            }
        }
    }
    "127.0.0.1:8765".to_string()
}

/// Wait for either Ctrl+C or SIGTERM (Unix only).
/// On non-Unix platforms only Ctrl+C is listened to.
///
/// Returns an error if the signal handler cannot be installed, which is
/// exceedingly rare in practice (would indicate fd exhaustion or
/// permission issues). Callers log the error and shut down rather than
/// `panic!`, so a misconfigured production environment still tears down
/// cleanly instead of aborting the process.
async fn shutdown_on_signal() -> Result<()> {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| anyhow::anyhow!("failed to install Ctrl+C handler: {e}"))
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| anyhow::anyhow!("failed to install SIGTERM handler: {e}"))?;
        sig.recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("SIGTERM stream ended unexpectedly"))
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<Result<()>>();

    tokio::select! {
        res = ctrl_c => { res.map(|_| ()) }
        res = terminate => { res.map(|_| ()) }
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
    "http://localhost:8765".to_string()
}
