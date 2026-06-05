use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::info;

use ilink_hub::{
    hub::{spawn_dispatcher, spawn_health_checker, HubState, InMemoryQueue, MessageQueue},
    ilink::{LoginClient, UpstreamClient},
    server::build_router,
    store::Store,
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
        #[arg(long, default_value = "0.0.0.0:8765", env = "ILINK_HUB_ADDR")]
        addr: String,

        /// Override real iLink base URL (for testing / custom deployments)
        #[arg(long, env = "ILINK_BASE_URL")]
        ilink_base_url: Option<String>,

        /// Database connection URL.
        /// Defaults to SQLite at ./ilink-hub.db
        /// Examples:
        ///   sqlite:///path/to/db.sqlite
        ///   postgres://user:pass@localhost/ilink_hub
        ///   mysql://user:pass@localhost/ilink_hub
        #[arg(long, default_value = "sqlite:./ilink-hub.db", env = "DATABASE_URL")]
        database_url: String,
    },

    /// Interactive QR-code login — scans WeChat and saves bot_token to DB
    Login {
        #[arg(long, default_value = "sqlite:./ilink-hub.db", env = "DATABASE_URL")]
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
            run_server(token, addr, ilink_base_url, database_url).await?;
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

async fn run_server(
    token_arg: Option<String>,
    addr: String,
    ilink_base_url: Option<String>,
    database_url: String,
) -> Result<()> {
    info!(%addr, %database_url, "iLink Hub starting");

    // Connect to database
    let store = Arc::new(Store::connect(&database_url).await?);

    // Resolve bot token: arg > env > DB > QR login
    let (token, base_url) = resolve_token(token_arg, ilink_base_url, store.clone()).await?;

    let upstream = Arc::new(UpstreamClient::new(token, Some(base_url)));
    let queue = build_queue_backend();
    let state = HubState::new(upstream.clone(), store.clone(), queue);

    // Pre-load persisted clients, routing state, and recent ctx_map into memory
    load_clients_from_db(state.clone(), store.clone()).await;

    // Upstream broadcast channel
    let (tx, rx) = broadcast::channel::<ilink_hub::ilink::types::WeixinMessage>(256);

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start dispatcher
    spawn_dispatcher(state.clone(), rx);

    // Start health checker
    spawn_health_checker(state.clone());

    // Start upstream polling loop
    {
        let upstream_clone = upstream.clone();
        let tx_clone = tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();
        tokio::spawn(async move {
            upstream_clone
                .run_polling_loop(tx_clone, shutdown_rx_clone)
                .await;
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

async fn resolve_token(
    token_arg: Option<String>,
    ilink_base_url: Option<String>,
    store: Arc<Store>,
) -> Result<(String, String)> {
    let default_base = "https://ilinkai.weixin.qq.com".to_string();

    if let Some(token) = token_arg {
        let base = ilink_base_url.unwrap_or(default_base);
        store.save_credentials(&token, &base).await?;
        return Ok((token, base));
    }

    // Try loading from DB
    if let Some((token, base)) = store.load_credentials().await? {
        info!("Loaded bot token from database");
        return Ok((token, base));
    }

    // Fall back to QR login
    info!("No token found, starting QR login");
    let login_client = LoginClient::new(ilink_base_url.clone());
    let token = login_client.login_with_qr().await?;
    let base = ilink_base_url.unwrap_or(default_base);
    store.save_credentials(&token, &base).await?;
    Ok((token, base))
}

async fn load_clients_from_db(state: Arc<HubState>, store: Arc<Store>) {
    // Load registered clients (preserving stored vtokens)
    // Queue entries are created on first push, so no explicit ensure() is needed.
    match store.list_clients().await {
        Ok(clients) => {
            let count = clients.len();
            let mut registry = state.registry.write().await;
            for c in clients {
                registry.register_with_vtoken(
                    c.name.clone(),
                    c.label.clone(),
                    Some(c.vtoken.clone()),
                );
            }
            info!(count, "loaded clients from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load clients from DB");
        }
    }

    // Restore routing state (from_user → vtoken)
    match store.list_routes().await {
        Ok(routes) => {
            let count = routes.len();
            let mut router = state.router.lock().await;
            for (from_user, vtoken) in routes {
                router.set_route(&from_user, vtoken);
            }
            info!(count, "loaded routing state from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load routing state from DB");
        }
    }

    // Warm in-memory context_token cache from DB (recent 500 entries)
    match store.list_recent_context_tokens(500).await {
        Ok(entries) => {
            let count = entries.len();
            let mut ctx_map = state.ctx_map.lock().await;
            for (vctx, real_ctx) in entries {
                ctx_map.seed(vctx, real_ctx);
            }
            info!(count, "warmed context_token cache from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load context_token cache from DB");
        }
    }
}

/// Select and initialise the queue backend from the `ILINK_QUEUE_BACKEND` env var.
///
/// Supported values:
/// - `"memory"` or unset — in-memory queue (default)
/// - `"redis"` — not yet implemented; falls back to memory with a warning
/// - anything else — falls back to memory with an error log
fn build_queue_backend() -> Arc<dyn MessageQueue> {
    match std::env::var("ILINK_QUEUE_BACKEND")
        .as_deref()
        .unwrap_or("")
    {
        "memory" | "" => {
            info!(backend = "memory", "queue backend initialized");
            Arc::new(InMemoryQueue::new())
        }
        "redis" => {
            tracing::warn!("Redis queue backend is not yet implemented; falling back to memory");
            Arc::new(InMemoryQueue::new())
        }
        other => {
            tracing::error!(
                backend = other,
                "Unknown ILINK_QUEUE_BACKEND; supported values: 'memory'. \
                 (redis: planned, not yet available). Falling back to memory."
            );
            Arc::new(InMemoryQueue::new())
        }
    }
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
