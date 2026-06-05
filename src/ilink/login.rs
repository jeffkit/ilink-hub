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
        let key = qr_resp
            .key
            .ok_or_else(|| anyhow!("no qrcode key in response"))?;
        let qr_url = qr_resp
            .qrcode
            .ok_or_else(|| anyhow!("no qrcode URL in response"))?;

        // Step 2: Render QR code in terminal
        println!("\n╔══════════════════════════════════════╗");
        println!("║     WeChat ClawBot Login              ║");
        println!("╚══════════════════════════════════════╝");
        println!();
        render_qr_terminal(&qr_url)?;
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

            match resp.status {
                Some(0) => {
                    // Still waiting
                }
                Some(1) => {
                    info!("QR code scanned, waiting for confirmation...");
                }
                Some(2) => {
                    // Confirmed / logged in
                    if let Some(token) = resp.bot_token {
                        info!("QR login successful");
                        return Ok(token);
                    }
                    return Err(anyhow!("login succeeded but no bot_token in response"));
                }
                Some(status) => {
                    return Err(anyhow!("unexpected qrcode status: {}", status));
                }
                None if resp.ret != 0 => {
                    return Err(anyhow!(
                        "qrcode status error: {}",
                        resp.errmsg.as_deref().unwrap_or("unknown")
                    ));
                }
                None => {}
            }
        }
    }
}

/// Render a URL as a QR code using Unicode block characters.
fn render_qr_terminal(url: &str) -> Result<()> {
    use qrcode::render::unicode;
    use qrcode::{EcLevel, QrCode};

    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L)
        .map_err(|e| anyhow!("QR code error: {e}"))?;

    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .build();

    println!("{}", image);
    Ok(())
}
