use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::info;

use ilink_hub::{
    ilink::LoginClient, paths::default_database_url, run_serve, store::Store, ServeOptions,
};

#[derive(Parser)]
#[command(name = "ilink-hub")]
#[command(version, about = "iLink-compatible multiplexer hub for WeChat ClawBot")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the hub server
    Serve {
        /// Real iLink bot token. If omitted, loaded from DATABASE_URL or triggers QR login.
        #[arg(long, env = "ILINK_TOKEN")]
        token: Option<String>,

        /// Hub listen address
        #[arg(long, default_value = "127.0.0.1:8765", env = "ILINK_HUB_ADDR")]
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

    /// Register a backend client with the hub (outputs vtoken to use)
    Register {
        /// Hub URL
        #[arg(long, default_value = "http://localhost:8765", env = "ILINK_HUB_URL")]
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
        #[arg(long, default_value = "http://localhost:8765", env = "ILINK_HUB_URL")]
        hub_url: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ilink_hub=debug".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            token,
            addr,
            ilink_base_url,
            database_url,
        } => {
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let shutdown_tx_clone = shutdown_tx.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("Received Ctrl+C, shutting down");
                    let _ = shutdown_tx_clone.send(true);
                }
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
    let resp: serde_json::Value = client
        .post(format!("{hub_url}/hub/register"))
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
    let resp: serde_json::Value = client
        .get(format!("{hub_url}/hub/clients"))
        .send()
        .await?
        .json()
        .await?;

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
