//! QR code login flow for the iLink Bot API.
//! Handles get_bot_qrcode → polling get_qrcode_status → returns bot_token.

use anyhow::{anyhow, Result};
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use super::types::*;

/// UI hints for embedders (e.g. Tauri) during WeChat QR login — safe to serialize to the webview.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum QrLoginUiEvent {
    #[serde(rename = "ready")]
    Ready { image: String, link: String },
    #[serde(rename = "status")]
    Status { message: String },
    #[serde(rename = "done")]
    Done,
    /// Sent when the QR code expires or the login attempt times out.
    #[serde(rename = "expired")]
    Expired,
}

pub struct LoginClient {
    client: Client,
    base_url: String,
}

impl LoginClient {
    pub fn new(base_url: Option<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("http client"),
            base_url: base_url.unwrap_or_else(|| ILINK_BASE_URL.to_string()),
        }
    }

    /// Full QR login flow — prints QR to terminal, polls until scanned.
    pub async fn login_with_qr(&self) -> Result<String> {
        self.login_with_qr_ui(None).await
    }

    /// Same as [`login_with_qr`], but can push QR + status to `ui` (e.g. a desktop window).
    pub async fn login_with_qr_ui(
        &self,
        ui: Option<UnboundedSender<QrLoginUiEvent>>,
    ) -> Result<String> {
        info!("Starting iLink QR login");

        let qr_resp = self.get_qrcode().await?;
        let key = qr_resp
            .qrcode
            .ok_or_else(|| anyhow!("no qrcode key in response"))?;
        let qr_url = qr_resp
            .qrcode_img_content
            .ok_or_else(|| anyhow!("no qrcode URL in response"))?;

        if let Some(ref tx) = ui {
            let image = crate::client::pairing::encode_qr_svg_data_uri(&qr_url)?;
            let _ = tx.send(QrLoginUiEvent::Ready {
                image,
                link: qr_url.clone(),
            });
        } else {
            println!("\n╔══════════════════════════════════════╗");
            println!("║     WeChat ClawBot Login              ║");
            println!("╚══════════════════════════════════════╝");
            println!();
            crate::client::pairing::render_qr_terminal(&qr_url)?;
            println!();
            println!("Scan the QR code with WeChat to log in.");
            println!("QR URL: {}", qr_url);
            println!();
        }

        let out = self.poll_qrcode_status(&key, ui.as_ref()).await;
        if let Some(tx) = &ui {
            // Only send Done on success; Expired is sent by poll_qrcode_status on failure.
            if out.is_ok() {
                let _ = tx.send(QrLoginUiEvent::Done);
            }
        }
        out
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

    async fn poll_qrcode_status(
        &self,
        key: &str,
        ui: Option<&UnboundedSender<QrLoginUiEvent>>,
    ) -> Result<String> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            self.base_url, key
        );
        let mut attempts = 0u32;
        // Each poll may be a long-poll (~30s server hold); allow up to 60 retries (~30min window).
        const MAX_ATTEMPTS: u32 = 60;

        loop {
            if attempts >= MAX_ATTEMPTS {
                if let Some(tx) = ui {
                    let _ = tx.send(QrLoginUiEvent::Expired);
                }
                return Err(anyhow!(
                    "QR login timed out after {} attempts",
                    MAX_ATTEMPTS
                ));
            }
            attempts += 1;

            tokio::time::sleep(Duration::from_secs(1)).await;

            let resp = match self.client.get(&url).send().await {
                Err(e) => {
                    warn!(error = %e, "network error polling qrcode status, retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Ok(r) => match r.json::<QrcodeStatusResponse>().await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, "error parsing qrcode status response, retrying");
                        continue;
                    }
                },
            };

            match resp.status.as_deref() {
                Some("wait") | None => {}
                Some("scaned") | Some("scanned") => {
                    info!("QR code scanned, waiting for confirmation...");
                    if let Some(tx) = ui {
                        let _ = tx.send(QrLoginUiEvent::Status {
                            message: "已在手机上扫码，请在微信里确认登录".into(),
                        });
                    }
                }
                Some("confirmed") => {
                    if let Some(token) = resp.bot_token {
                        info!("QR login successful");
                        return Ok(token);
                    }
                    return Err(anyhow!("login confirmed but no bot_token in response"));
                }
                Some("expired") => {
                    if let Some(tx) = ui {
                        let _ = tx.send(QrLoginUiEvent::Expired);
                    }
                    return Err(anyhow!("QR code expired, please run login again"));
                }
                Some(status) => {
                    if resp.ret != 0 {
                        return Err(anyhow!(
                            "qrcode status error: {}",
                            resp.errmsg.as_deref().unwrap_or(status)
                        ));
                    }
                    warn!(status, "unknown qrcode status, continuing to poll");
                }
            }
        }
    }
}
