use std::sync::Arc;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::{broadcast, watch};
use tracing::info;

use ilink_hub::{
    hub::{spawn_dispatcher, HubState},
    ilink::UpstreamClient,
    server::build_router,
};

#[derive(Parser)]
#[command(name = "ilink-hub")]
#[command(about = "iLink-compatible multiplexer hub for WeChat ClawBot", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the hub server
    Serve {
        /// Real iLink bot token (from WeChat ClawBot login)
        #[arg(long, env = "ILINK_TOKEN")]
        token: String,

        /// Hub listen address
        #[arg(long, default_value = "0.0.0.0:8765", env = "ILINK_HUB_ADDR")]
        addr: String,

        /// Override real iLink base URL (for testing)
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
        Commands::Serve { token, addr, ilink_base_url } => {
            run_server(token, addr, ilink_base_url).await?;
        }
        Commands::Register { hub_url, name, label } => {
            register_client(&hub_url, name, label).await?;
        }
        Commands::Clients { hub_url } => {
            list_clients(&hub_url).await?;
        }
    }

    Ok(())
}

async fn run_server(token: String, addr: String, ilink_base_url: Option<String>) -> Result<()> {
    info!(%addr, "iLink Hub starting");

    let upstream = Arc::new(UpstreamClient::new(token, ilink_base_url));
    let state = HubState::new(upstream.clone());

    // Upstream broadcast channel: capacity 256 messages
    let (tx, rx) = broadcast::channel::<ilink_hub::ilink::types::InboundMessage>(256);

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start dispatcher (routes upstream messages to client queues)
    spawn_dispatcher(state.clone(), rx);

    // Start upstream polling loop
    {
        let upstream_clone = upstream.clone();
        let tx_clone = tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();
        tokio::spawn(async move {
            upstream_clone.run_polling_loop(tx_clone, shutdown_rx_clone).await;
        });
    }

    // Handle Ctrl+C
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received Ctrl+C, shutting down");
            let _ = shutdown_tx_clone.send(true);
        }
    });

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(%addr, "iLink Hub listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let mut rx = shutdown_rx;
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await?;

    info!("iLink Hub stopped");
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
        println!("   Virtual token: {vtoken}");
        println!();
        println!("Configure your backend:");
        println!("   WEIXIN_BASE_URL={hub_url}");
        println!("   WEIXIN_TOKEN={vtoken}");
    } else {
        eprintln!("❌ Registration failed: {}", resp["errmsg"].as_str().unwrap_or("unknown"));
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
        println!("{:<20} {:<10} {}", "NAME", "STATUS", "LABEL");
        println!("{}", "-".repeat(50));
        for c in clients {
            let name = c["name"].as_str().unwrap_or("-");
            let label = c["label"].as_str().unwrap_or("-");
            let online = if c["online"].as_bool().unwrap_or(false) { "🟢 online" } else { "🔴 offline" };
            println!("{:<20} {:<10} {}", name, online, label);
        }
    }

    Ok(())
}
