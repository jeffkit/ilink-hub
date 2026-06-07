//! QR code login flow for the iLink Bot API.
//! Handles get_bot_qrcode → polling get_qrcode_status → returns bot_token.

use anyhow::{anyhow, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::{info, warn};

use super::types::*;

pub struct LoginClient {
    client: Client,
    base_url: String,
}

impl LoginClient {
    pub fn new(base_url: Option<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            base_url: base_url.unwrap_or_else(|| ILINK_BASE_URL.to_string()),
        }
    }

    /// Full QR login flow — prints QR to terminal, polls until scanned.
    /// Returns (bot_token, bot_type) on success.
    pub async fn login_with_qr(&self) -> Result<String> {
        info!("Starting iLink QR login");

        // Step 1: Get QR code
        let qr_resp = self.get_qrcode().await?;
        // `qrcode` is the key/identifier used for polling
        let key = qr_resp
            .qrcode
            .ok_or_else(|| anyhow!("no qrcode key in response"))?;
        // `qrcode_img_content` is the URL to render as a QR code
        let qr_url = qr_resp
            .qrcode_img_content
            .ok_or_else(|| anyhow!("no qrcode URL in response"))?;

        // Step 2: Render QR code in terminal
        println!("\n╔══════════════════════════════════════╗");
        println!("║     WeChat ClawBot Login              ║");
        println!("╚══════════════════════════════════════╝");
        println!();
        crate::client::pairing::render_qr_terminal(&qr_url)?;
        println!();
        println!("Scan the QR code with WeChat to log in.");
        println!("QR URL: {}", qr_url);
        println!();

        // Step 3: Poll for scan
        self.poll_qrcode_status(&key).await
    }

    async fn get_qrcode(&self) -> Result<GetQrcodeResponse> {
        let url = format!("{}/ilink/bot/get_bot_qrcode?bot_type=3", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .json::<GetQrcodeResponse>()
            .await?;
        if resp.ret != 0 {
            return Err(anyhow!(
                "get_bot_qrcode failed: {}",
                resp.errmsg.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(resp)
    }

    async fn poll_qrcode_status(&self, key: &str) -> Result<String> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            self.base_url, key
        );
        let mut attempts = 0u32;
        const MAX_ATTEMPTS: u32 = 120; // 2 minutes

        loop {
            if attempts >= MAX_ATTEMPTS {
                return Err(anyhow!("QR login timed out (120s)"));
            }
            attempts += 1;

            tokio::time::sleep(Duration::from_secs(1)).await;

            let resp = match self
                .client
                .get(&url)
                .send()
                .await?
                .json::<QrcodeStatusResponse>()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "error polling qrcode status");
                    continue;
                }
            };

            // API returns status as a string: "wait" | "confirmed" | "expired"
            match resp.status.as_deref() {
                Some("wait") | None => {
                    // Still waiting for scan
                }
                Some("scaned") | Some("scanned") => {
                    info!("QR code scanned, waiting for confirmation...");
                }
                Some("confirmed") => {
                    if let Some(token) = resp.bot_token {
                        info!("QR login successful");
                        return Ok(token);
                    }
                    return Err(anyhow!("login confirmed but no bot_token in response"));
                }
                Some("expired") => {
                    return Err(anyhow!("QR code expired, please run login again"));
                }
                Some(status) => {
                    if resp.ret != 0 {
                        return Err(anyhow!(
                            "qrcode status error: {}",
                            resp.errmsg.as_deref().unwrap_or(status)
                        ));
                    }
                    // Unknown status — log and keep polling
                    warn!(status, "unknown qrcode status, continuing to poll");
                }
            }
        }
    }
}
