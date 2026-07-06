//! QR code login flow for the iLink Bot API.
//! Handles get_bot_qrcode → polling get_qrcode_status → returns bot_token.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::error::HubError;

use super::types::*;

/// Map a JSON parse error from the QR login response decode into a
/// `HubError::UpstreamParse` so the N-06 specific variant survives a
/// round-trip through `anyhow::Result` and is observable to downstream
/// `HubError` consumers. See `crate::error::From<anyhow::Error>` for
/// the downcast that recovers this variant.
fn upstream_parse_err(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(HubError::UpstreamParse(e.to_string()))
}

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
    pub fn new(base_url: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.unwrap_or_else(|| ILINK_BASE_URL.to_string()),
        })
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
            .await
            .map_err(upstream_parse_err)?;
        if resp.ret != 0 {
            return Err(anyhow!(
                "get_bot_qrcode failed: {}",
                resp.errmsg.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(resp)
    }

    #[cfg(test)]
    fn with_base_url(base_url: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { client, base_url })
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

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    fn make_client(base_url: String) -> LoginClient {
        LoginClient::with_base_url(base_url).expect("test client")
    }

    /// M8-login-1: upstream_parse_err 必须将错误包装为 HubError::UpstreamParse。
    /// 捕捉 `anyhow::Error::new(HubError::UpstreamParse(...))` → 其他错误类型的变异。
    #[test]
    fn upstream_parse_err_wraps_as_hub_error_upstream_parse() {
        let err = upstream_parse_err("json parse failed");
        let hub_err = err
            .downcast::<HubError>()
            .expect("must downcast to HubError");
        assert!(
            matches!(hub_err, HubError::UpstreamParse(_)),
            "must be HubError::UpstreamParse, got: {hub_err:?}"
        );
    }

    /// M8-login-2: get_qrcode 当 ret != 0 时必须返回 Err。
    /// 捕捉 `resp.ret != 0` → `resp.ret == 0` 的变异——若取反则成功路径变成失败路径。
    #[tokio::test]
    async fn get_qrcode_non_zero_ret_returns_err() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_bot_qrcode?bot_type=3")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":1,"errmsg":"auth failed","qrcode":null,"qrcode_img_content":null}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.get_qrcode().await;
        assert!(result.is_err(), "non-zero ret must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("get_bot_qrcode failed"),
            "error message must mention get_bot_qrcode, got: {msg}"
        );
    }

    /// M8-login-3: get_qrcode 当 ret == 0 时必须返回 Ok，并包含 qrcode 字段。
    #[tokio::test]
    async fn get_qrcode_zero_ret_returns_ok() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_bot_qrcode?bot_type=3")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"qrcode":"test-key-123","qrcode_img_content":"https://example.com/qr.png"}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.get_qrcode().await;
        assert!(result.is_ok(), "zero ret must return Ok, got: {result:?}");
        let resp = result.unwrap();
        assert_eq!(resp.qrcode.as_deref(), Some("test-key-123"));
    }

    /// M8-login-4: poll_qrcode_status 当响应状态为 "confirmed" 且含 bot_token 时必须返回 Ok(token)。
    /// 捕捉 `Some("confirmed")` 分支被替换或 bot_token 检查被 no-op 的变异。
    #[tokio::test]
    async fn poll_qrcode_status_confirmed_with_token_returns_ok() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_qrcode_status?qrcode=test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"status":"confirmed","bot_token":"my-bot-token-xyz","baseurl":null,"ilink_bot_id":null,"ilink_user_id":null}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.poll_qrcode_status("test-key", None).await;
        assert!(
            result.is_ok(),
            "confirmed status must return Ok, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "my-bot-token-xyz");
    }

    /// M8-login-5: poll_qrcode_status 当 "confirmed" 但无 bot_token 时必须返回 Err。
    #[tokio::test]
    async fn poll_qrcode_status_confirmed_without_token_returns_err() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_qrcode_status?qrcode=no-token-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"status":"confirmed","bot_token":null,"baseurl":null,"ilink_bot_id":null,"ilink_user_id":null}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.poll_qrcode_status("no-token-key", None).await;
        assert!(
            result.is_err(),
            "confirmed without bot_token must return Err"
        );
    }

    /// M8-login-6: poll_qrcode_status 当状态为 "expired" 时必须返回 Err。
    /// 捕捉 `Some("expired")` 分支被跳过或 Err 变成 Ok 的变异。
    #[tokio::test]
    async fn poll_qrcode_status_expired_returns_err() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_qrcode_status?qrcode=expired-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"status":"expired","bot_token":null,"baseurl":null,"ilink_bot_id":null,"ilink_user_id":null}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.poll_qrcode_status("expired-key", None).await;
        assert!(result.is_err(), "expired status must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("expired"),
            "error must mention 'expired', got: {msg}"
        );
    }

    /// M8-login-7: poll_qrcode_status 当 ret != 0 且状态未知时必须返回 Err。
    /// 捕捉 `if resp.ret != 0` → `if resp.ret == 0` 的变异。
    #[tokio::test]
    async fn poll_qrcode_status_unknown_status_nonzero_ret_returns_err() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/ilink/bot/get_qrcode_status?qrcode=err-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":5,"status":"unknown_status","errmsg":"something went wrong","bot_token":null,"baseurl":null,"ilink_bot_id":null,"ilink_user_id":null}"#,
            )
            .create_async()
            .await;

        let client = make_client(server.url());
        let result = client.poll_qrcode_status("err-key", None).await;
        assert!(
            result.is_err(),
            "non-zero ret with unknown status must return Err"
        );
    }
}
