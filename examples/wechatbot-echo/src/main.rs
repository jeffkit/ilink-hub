//! Minimal echo client that connects to iLink Hub via the `wechatbot` crate.
//!
//! **Zero-config pairing (OpenClaw-style):** set only `WEIXIN_BASE_URL`, run
//! `cargo run` — terminal shows a QR code; scan with your phone to pair.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use ilink_hub::client::{HubPairingClient, HubPairingOptions};
use wechatbot::{BotOptions, ContentType, Credentials, WeChatBot};

#[derive(Parser, Debug)]
#[command(name = "wechatbot-echo", about = "Echo test client for iLink Hub")]
struct Args {
    /// Hub base URL (WEIXIN_BASE_URL). Only config needed for first-time QR pairing.
    #[arg(long, env = "WEIXIN_BASE_URL", default_value = "http://127.0.0.1:8765")]
    hub_url: String,

    /// Virtual token — skip if pairing via QR (WEIXIN_TOKEN)
    #[arg(long, env = "WEIXIN_TOKEN")]
    token: Option<String>,

    /// wechatbot credential cache (also used after Hub QR pairing)
    #[arg(long, default_value = ".wechatbot-hub-credentials.json")]
    cred_path: String,

    /// Force a new Hub pairing QR even when credentials exist
    #[arg(long)]
    force_pair: bool,

    /// Prefix prepended to echoed replies
    #[arg(long, default_value = "Echo: ")]
    reply_prefix: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wechatbot=debug".into()),
        )
        .init();

    let args = Args::parse();
    let hub_url = args.hub_url.trim_end_matches('/').to_string();

    let hub_creds = if let Some(token) = args.token.filter(|t| !t.trim().is_empty()) {
        tracing::info!("using WEIXIN_TOKEN from env/CLI");
        write_hub_credentials(&args.cred_path, &hub_url, &token).await?;
        ilink_hub::client::HubPairingCredentials {
            token,
            base_url: hub_url.clone(),
            account_id: "ilink-hub-client".to_string(),
            user_id: "hub-client".to_string(),
            saved_at: None,
            client_name: None,
        }
    } else {
        tracing::info!(%hub_url, "no token — starting Hub QR pairing");
        let pairing = HubPairingClient::new(HubPairingOptions {
            hub_url: hub_url.clone(),
            cred_path: Some(args.cred_path.clone()),
            force: args.force_pair,
            bot_type: "3".to_string(),
        });
        pairing.pair().await.context("hub QR pairing failed")?
    };

    sync_wechatbot_credentials(&args.cred_path, &hub_creds).await?;

    let bot = Arc::new(WeChatBot::new(BotOptions {
        base_url: Some(hub_creds.base_url.clone()),
        cred_path: Some(args.cred_path),
        on_error: Some(Box::new(|err| {
            tracing::error!(%err, "wechatbot error");
        })),
        ..Default::default()
    }));

    let creds = bot.login(false).await.context("load hub credentials")?;
    tracing::info!(
        hub = %hub_creds.base_url,
        account = %creds.account_id,
        "connected to iLink Hub"
    );

    let reply_prefix = args.reply_prefix;
    let handler_bot = bot.clone();
    bot.on_message(Box::new(move |msg| {
        if msg.content_type != ContentType::Text {
            tracing::info!(user = %msg.user_id, kind = ?msg.content_type, "non-text message, skip echo");
            return;
        }

        tracing::info!(user = %msg.user_id, text = %msg.text, "received");
        let bot = handler_bot.clone();
        let reply = format!("{reply_prefix}{}", msg.text);
        let msg = msg.clone();

        tokio::spawn(async move {
            if let Err(err) = bot.reply(&msg, &reply).await {
                tracing::error!(%err, user = %msg.user_id, "reply failed");
            } else {
                tracing::debug!(user = %msg.user_id, %reply, "replied");
            }
        });
    }))
    .await;

    tracing::info!("long-polling /ilink/bot/getupdates — send a WeChat message to test");
    bot.run().await.context("poll loop exited")?;
    Ok(())
}

async fn write_hub_credentials(path: &str, hub_url: &str, token: &str) -> anyhow::Result<()> {
    let creds = ilink_hub::client::HubPairingCredentials {
        token: token.to_string(),
        base_url: hub_url.to_string(),
        account_id: "ilink-hub-client".to_string(),
        user_id: "hub-client".to_string(),
        saved_at: None,
        client_name: None,
    };
    save_json_credentials(path, &creds).await
}

async fn sync_wechatbot_credentials(
    path: &str,
    hub: &ilink_hub::client::HubPairingCredentials,
) -> anyhow::Result<()> {
    let creds = Credentials {
        token: hub.token.clone(),
        base_url: hub.base_url.clone(),
        account_id: hub.account_id.clone(),
        user_id: hub.user_id.clone(),
        saved_at: hub.saved_at.clone(),
    };
    save_json_credentials(path, &creds).await
}

async fn save_json_credentials(path: &str, creds: &impl serde::Serialize) -> anyhow::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent().filter(|p| !p.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(creds)?;
    tokio::fs::write(path, format!("{json}\n")).await?;
    Ok(())
}
