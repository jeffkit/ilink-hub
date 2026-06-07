//! Hub client pairing — OpenClaw-style terminal QR + phone confirmation.
//!
//! Calls `get_bot_qrcode` / `get_qrcode_status` on the configured Hub `base_url`
//! (not `ilinkai.weixin.qq.com`). The QR encodes the public pair URL (`ilinkhub.ai`).

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

use crate::ilink::types::{GetQrcodeResponse, QrcodeStatusResponse};

const ILINK_APP_ID: &str = "bot";
const DEFAULT_BOT_TYPE: &str = "3";
const MAX_QR_REFRESH: u32 = 3;
const STATUS_POLL_TIMEOUT: Duration = Duration::from_secs(40);
const PAIRING_DEADLINE: Duration = Duration::from_secs(480);

/// Credentials returned after a successful Hub pairing session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HubPairingCredentials {
    pub token: String,
    pub base_url: String,
    pub account_id: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<String>,
}

/// Options for [`HubPairingClient::pair`].
#[derive(Debug, Clone)]
pub struct HubPairingOptions {
    pub hub_url: String,
    pub cred_path: Option<String>,
    pub force: bool,
    pub bot_type: String,
}

impl HubPairingOptions {
    pub fn new(hub_url: impl Into<String>) -> Self {
        Self {
            hub_url: hub_url.into().trim_end_matches('/').to_string(),
            cred_path: None,
            force: false,
            bot_type: DEFAULT_BOT_TYPE.to_string(),
        }
    }
}

pub struct HubPairingClient {
    http: Client,
    opts: HubPairingOptions,
}

impl HubPairingClient {
    pub fn new(opts: HubPairingOptions) -> Self {
        Self {
            http: Client::builder()
                .timeout(STATUS_POLL_TIMEOUT)
                .build()
                .expect("http client"),
            opts,
        }
    }

    /// Load stored credentials or run the full QR pairing flow.
    pub async fn pair(&self) -> Result<HubPairingCredentials> {
        if !self.opts.force {
            if let Some(creds) = self.load_credentials().await? {
                info!(path = ?self.cred_path(), "loaded hub pairing credentials");
                return Ok(creds);
            }
        }

        self.pair_with_qr().await
    }

    /// Always run QR pairing (ignores stored credentials unless `force` refresh mid-flow).
    pub async fn pair_with_qr(&self) -> Result<HubPairingCredentials> {
        let mut qr_refresh = 0u32;

        loop {
            qr_refresh += 1;
            if qr_refresh > MAX_QR_REFRESH {
                return Err(anyhow!(
                    "pairing QR expired {MAX_QR_REFRESH} times — aborted"
                ));
            }

            let qr = self.fetch_qrcode().await?;
            let key = qr
                .qrcode
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("get_bot_qrcode: missing qrcode key"))?;
            let qr_url = qr
                .qrcode_img_content
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("get_bot_qrcode: missing qrcode_img_content"))?;

            self.print_pairing_banner(qr_refresh)?;
            render_qr_terminal(qr_url)?;
            println!();
            println!("用手机扫描上方二维码，在浏览器中确认客户端配对。");
            println!("若二维码无法扫描，可直接在手机浏览器打开：");
            println!("  {qr_url}");
            println!();

            match self.poll_until_confirmed(key).await {
                Ok(creds) => {
                    self.save_credentials(&creds).await?;
                    println!("✅ 配对成功，虚拟 Token 已保存。");
                    return Ok(creds);
                }
                Err(e) if e.to_string().contains("expired") => {
                    warn!("pairing QR expired, refreshing ({qr_refresh}/{MAX_QR_REFRESH})");
                    println!("⏳ 二维码已过期，正在刷新…");
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn cred_path(&self) -> String {
        self.opts
            .cred_path
            .clone()
            .unwrap_or_else(default_cred_path)
    }

    async fn load_credentials(&self) -> Result<Option<HubPairingCredentials>> {
        let path = self.cred_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn save_credentials(&self, creds: &HubPairingCredentials) -> Result<()> {
        let path = self.cred_path();
        if let Some(parent) = Path::new(&path).parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_string_pretty(creds)?;
        tokio::fs::write(&path, format!("{data}\n")).await?;
        Ok(())
    }

    fn print_pairing_banner(&self, attempt: u32) -> Result<()> {
        println!();
        println!("╔══════════════════════════════════════════╗");
        println!("║       iLink Hub — 客户端扫码配对          ║");
        println!("╚══════════════════════════════════════════╝");
        println!("Hub: {}", self.opts.hub_url);
        if attempt > 1 {
            println!("（第 {attempt} 次二维码）");
        }
        Ok(())
    }

    async fn fetch_qrcode(&self) -> Result<GetQrcodeResponse> {
        let url = format!(
            "{}/ilink/bot/get_bot_qrcode?bot_type={}",
            self.opts.hub_url, self.opts.bot_type
        );

        // OpenClaw uses POST with `local_token_list`; try POST first, then GET.
        let post_resp = self
            .http
            .post(&url)
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "local_token_list": [] }))
            .send()
            .await;

        let resp = match post_resp {
            Ok(r) if r.status().is_success() => r,
            _ => self
                .http
                .get(&url)
                .header("iLink-App-Id", ILINK_APP_ID)
                .send()
                .await
                .context("get_bot_qrcode request failed")?,
        };

        let body: GetQrcodeResponse = resp.json().await.context("parse get_bot_qrcode")?;
        if body.ret != 0 {
            return Err(anyhow!(
                "get_bot_qrcode failed: {}",
                body.errmsg.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(body)
    }

    async fn poll_until_confirmed(&self, qrcode_key: &str) -> Result<HubPairingCredentials> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={qrcode_key}",
            self.opts.hub_url
        );
        let deadline = tokio::time::Instant::now() + PAIRING_DEADLINE;
        let mut scaned_logged = false;

        while tokio::time::Instant::now() < deadline {
            let resp = self
                .http
                .get(&url)
                .header("iLink-App-Id", ILINK_APP_ID)
                .timeout(STATUS_POLL_TIMEOUT)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "poll get_qrcode_status failed, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let body: QrcodeStatusResponse = resp.json().await.context("parse qrcode status")?;

            match body.status.as_deref() {
                Some("wait") | None => {}
                Some("scaned") | Some("scanned") => {
                    if !scaned_logged {
                        println!("📱 已扫码，请在手机上确认配对…");
                        scaned_logged = true;
                    }
                }
                Some("confirmed") => {
                    let token = body
                        .bot_token
                        .filter(|t| !t.is_empty())
                        .ok_or_else(|| anyhow!("confirmed but bot_token missing"))?;
                    let base_url = body
                        .baseurl
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| self.opts.hub_url.clone());
                    return Ok(HubPairingCredentials {
                        token,
                        base_url,
                        account_id: body
                            .ilink_bot_id
                            .unwrap_or_else(|| "ilink-hub@hub.local".to_string()),
                        user_id: body.ilink_user_id.unwrap_or_else(|| "hub-client".to_string()),
                        saved_at: Some(chrono_now()),
                    });
                }
                Some("expired") => {
                    return Err(anyhow!("pairing session expired"));
                }
                Some(status) => {
                    warn!(%status, "unknown qrcode status, continuing");
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Err(anyhow!("pairing timed out after {}s", PAIRING_DEADLINE.as_secs()))
    }
}

/// Render a URL as a terminal QR code (same approach as OpenClaw `qrcode-terminal`).
pub fn render_qr_terminal(url: &str) -> Result<()> {
    use qrcode::render::unicode;
    use qrcode::{EcLevel, QrCode};

    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L)
        .map_err(|e| anyhow!("QR encode error: {e}"))?;
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .build();
    println!("{image}");
    Ok(())
}

fn default_cred_path() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| ".".into())
        .join(".ilink-hub")
        .join("client-credentials.json")
        .to_string_lossy()
        .to_string()
}

fn chrono_now() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    format!("{}Z", dur.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_qr_does_not_panic() {
        render_qr_terminal("https://ilinkhub.ai/pair/test/pair_abc").unwrap();
    }
}
