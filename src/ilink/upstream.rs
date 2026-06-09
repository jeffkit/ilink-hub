//! Upstream iLink client — connects to the real `ilinkai.weixin.qq.com`
//! and fans received messages out to the Hub's internal message bus.

use anyhow::Result;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tracing::{debug, error, info, warn};

use crate::store::Store;

use super::login::{LoginClient, QrLoginUiEvent};
use super::types::*;

/// Context for renewing an expired iLink session from the upstream polling loop.
pub struct SessionRenewal {
    pub store: Arc<Store>,
    pub ilink_base_url: Option<String>,
    pub qr_login_ui: Option<UnboundedSender<QrLoginUiEvent>>,
}

pub struct UpstreamClient {
    client: Client,
    base_url: String,
    token: RwLock<String>,
    pub polls_ok: AtomicU64,
    pub polls_err: AtomicU64,
}

impl UpstreamClient {
    pub fn new(token: String, base_url: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(70))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| ILINK_BASE_URL.to_string()),
            token: RwLock::new(token),
            polls_ok: AtomicU64::new(0),
            polls_err: AtomicU64::new(0),
        }
    }

    /// Local format check only — does not contact iLink.
    pub fn is_well_formed_bot_token(token: &str) -> bool {
        !token.is_empty() && token.contains(':')
    }

    pub fn set_token(&self, token: String) {
        *self.token.write().expect("upstream token lock poisoned") = token;
    }

    /// Extracts the bot ID from the token (`botid@im.bot:secretkey` → `botid@im.bot`).
    pub fn bot_id(&self) -> String {
        let token = self.token.read().expect("upstream token lock poisoned");
        token.split(':').next().unwrap_or("").to_string()
    }

    /// Calls `notifystart` — required before the bot can send messages.
    pub async fn notify_start(&self) -> Result<()> {
        let url = format!("{}/ilink/bot/msg/notifystart", self.base_url);
        let body = serde_json::json!({ "base_info": BaseInfo::default() });
        let _ = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;
        Ok(())
    }

    /// `X-WECHAT-UIN`: random uint32 as decimal string, then base64-encoded.
    fn random_uin(&self) -> String {
        use base64::Engine;
        use rand::Rng;
        let uint32: u32 = rand::thread_rng().gen();
        base64::engine::general_purpose::STANDARD.encode(uint32.to_string().as_bytes())
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
        let mut headers = HeaderMap::new();
        // Required by iLink: must be "ilink_bot_token" or session times out immediately
        headers.insert(
            "AuthorizationType",
            HeaderValue::from_static("ilink_bot_token"),
        );
        let token = self.token.read().expect("upstream token lock poisoned");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", *token)).unwrap(),
        );
        headers.insert(
            "X-WECHAT-UIN",
            HeaderValue::from_str(&self.random_uin()).unwrap(),
        );
        headers
    }

    /// Long-poll for new messages. Pass `timeout: Some(0)` for an immediate probe (e.g. session check).
    pub async fn get_updates(
        &self,
        get_updates_buf: String,
        timeout: Option<u32>,
    ) -> Result<GetUpdatesResponse> {
        let url = format!("{}/ilink/bot/getupdates", self.base_url);
        let req_body = GetUpdatesRequest {
            get_updates_buf,
            base_info: Some(BaseInfo::default()),
            timeout,
        };
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req_body)
            .send()
            .await?
            .json::<GetUpdatesResponse>()
            .await?;
        Ok(resp)
    }

    pub async fn send_message(&self, mut req: SendMessageRequest) -> Result<SendMessageResponse> {
        let url = format!("{}/ilink/bot/sendmessage", self.base_url);
        if let Some(msg) = &mut req.msg {
            msg.ensure_outbound();
        }
        if req.base_info.is_none() {
            req.base_info = Some(BaseInfo::default());
        }
        // The real API returns an empty body on success; parse loosely
        let text = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?
            .text()
            .await?;
        debug!(response = %text, "send_message raw response");
        // Empty body means success
        if text.trim().is_empty() {
            return Ok(SendMessageResponse::ok());
        }
        match serde_json::from_str::<SendMessageResponse>(&text) {
            Ok(resp) => {
                if resp.ret.map(|r| r != 0).unwrap_or(false) {
                    warn!(
                        ret = resp.ret,
                        errmsg = ?resp.errmsg,
                        "iLink sendmessage returned non-zero ret"
                    );
                }
                Ok(resp)
            }
            Err(_) => Ok(SendMessageResponse::ok()), // treat unparseable as success
        }
    }

    pub async fn send_typing(&self, req: SendTypingRequest) -> Result<()> {
        let url = format!("{}/ilink/bot/sendtyping", self.base_url);
        let _ = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?;
        Ok(())
    }

    pub async fn get_config(&self, req: GetConfigRequest) -> Result<GetConfigResponse> {
        let url = format!("{}/ilink/bot/getconfig", self.base_url);
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?
            .json::<GetConfigResponse>()
            .await?;
        Ok(resp)
    }

    pub async fn get_upload_url(&self, req: GetUploadUrlRequest) -> Result<GetUploadUrlResponse> {
        let url = format!("{}/ilink/bot/getuploadurl", self.base_url);
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?
            .json::<GetUploadUrlResponse>()
            .await?;
        Ok(resp)
    }

    /// Continuous polling loop — sends received messages to `tx`.
    pub async fn run_polling_loop(
        self: Arc<Self>,
        tx: broadcast::Sender<WeixinMessage>,
        shutdown: tokio::sync::watch::Receiver<bool>,
        renewal: Option<SessionRenewal>,
    ) {
        let mut get_updates_buf = String::new();
        let mut backoff_secs = 1u64;
        let renewing = Arc::new(AtomicBool::new(false));

        info!("iLink upstream polling started");

        // notifystart enables outbound message sending for this bot session
        match self.notify_start().await {
            Ok(_) => info!("iLink notifystart successful"),
            Err(e) => warn!(error = %e, "notifystart failed — outbound messages may not work"),
        }

        loop {
            if *shutdown.borrow() {
                info!("iLink upstream polling shutting down");
                return;
            }

            let result = self.get_updates(get_updates_buf.clone(), None).await;

            match result {
                Ok(resp) if resp.ret == Some(0) || resp.errcode.is_none() => {
                    self.polls_ok.fetch_add(1, Ordering::Relaxed);
                    backoff_secs = 1;
                    if let Some(new_buf) = resp.get_updates_buf {
                        if !new_buf.is_empty() {
                            get_updates_buf = new_buf;
                        }
                    }
                    if let Some(messages) = resp.msgs {
                        for msg in messages {
                            debug!(
                                from = msg.from_user_id.as_deref().unwrap_or("?"),
                                ctx = msg.context_token.as_deref().unwrap_or("(none)"),
                                text = msg.text().unwrap_or("(none)"),
                                has_item_list = msg.item_list.is_some(),
                                "received upstream message"
                            );
                            let _ = tx.send(msg);
                        }
                    }
                }
                Ok(resp) => {
                    self.polls_err.fetch_add(1, Ordering::Relaxed);
                    let code = resp.errcode.or(resp.ret).unwrap_or(-1);
                    warn!(
                        code,
                        errmsg = ?resp.errmsg,
                        "iLink upstream returned error"
                    );
                    if code == -14 {
                        if let Some(ref renewal_ctx) = renewal {
                            if renewing
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                match renew_expired_session(self.clone(), renewal_ctx).await {
                                    Ok(()) => {
                                        backoff_secs = 1;
                                        get_updates_buf.clear();
                                        renewing.store(false, Ordering::SeqCst);
                                        continue;
                                    }
                                    Err(e) => {
                                        error!(error = %e, "iLink session renewal failed");
                                        renewing.store(false, Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                }
                Err(e) => {
                    self.polls_err.fetch_add(1, Ordering::Relaxed);
                    error!(error = %e, "iLink upstream request failed");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                }
            }
        }
    }
}

async fn renew_expired_session(
    upstream: Arc<UpstreamClient>,
    renewal: &SessionRenewal,
) -> Result<()> {
    let quiet_ui = renewal.qr_login_ui.is_some();
    if !quiet_ui {
        println!();
        println!("⚠️  iLink 微信登录态已过期，请扫描下方二维码重新登录。");
        println!();
    }
    warn!("iLink session expired (-14), starting QR re-login");

    let login_client = LoginClient::new(renewal.ilink_base_url.clone());
    let token = login_client
        .login_with_qr_ui(renewal.qr_login_ui.clone())
        .await?;
    let base = renewal
        .ilink_base_url
        .clone()
        .unwrap_or_else(|| ILINK_BASE_URL.to_string());

    renewal.store.save_credentials(&token, &base).await?;
    upstream.set_token(token);
    info!("iLink session renewed, token saved");

    match upstream.notify_start().await {
        Ok(_) => info!("iLink notifystart successful after renewal"),
        Err(e) => warn!(error = %e, "notifystart failed after renewal — outbound messages may not work"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::UpstreamClient;

    #[test]
    fn well_formed_bot_token() {
        assert!(UpstreamClient::is_well_formed_bot_token("bot@im.bot:secret"));
        assert!(!UpstreamClient::is_well_formed_bot_token(""));
        assert!(!UpstreamClient::is_well_formed_bot_token("no-colon"));
    }
}
