//! Upstream iLink client — connects to the real `ilinkai.weixin.qq.com`
//! and fans received messages out to the Hub's internal message bus.

use anyhow::Result;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
    /// Desktop UI channel (Tauri).
    pub qr_login_ui: Option<UnboundedSender<QrLoginUiEvent>>,
    /// Web admin SSE broadcast.
    pub qr_tx: Option<broadcast::Sender<QrLoginUiEvent>>,
    /// Cache of the last QR Ready event for late SSE subscribers.
    pub qr_last_ready: Option<Arc<tokio::sync::Mutex<Option<QrLoginUiEvent>>>>,
    /// Current connection status (see `crate::hub::ilink_status`).
    pub ilink_status: Option<Arc<AtomicU8>>,
    /// Receives manual re-login triggers from the admin API.
    pub relogin_rx: Option<broadcast::Receiver<()>>,
}

pub struct UpstreamClient {
    client: Client,
    base_url: String,
    token: RwLock<String>,
    pub polls_ok: AtomicU64,
    pub polls_err: AtomicU64,
    pub relogin_attempts: AtomicU64,
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
            relogin_attempts: AtomicU64::new(0),
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
            .await?
            .error_for_status()?;
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
        mut renewal: Option<SessionRenewal>,
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

        // Probe session validity immediately with a zero-timeout poll.
        // This catches -14 right at startup instead of waiting for the first real poll.
        match self.get_updates(String::new(), Some(0)).await {
            Ok(resp) => {
                let code = resp.errcode.or(resp.ret).unwrap_or(0);
                if code == -14 {
                    warn!("startup session probe returned -14, triggering immediate re-login");
                    if let Some(ref renewal_ctx) = renewal {
                        set_status(renewal_ctx, crate::hub::ilink_status::NEEDS_LOGIN);
                        match renew_expired_session(self.clone(), renewal_ctx).await {
                            Ok(()) => {
                                set_status(renewal_ctx, crate::hub::ilink_status::CONNECTED);
                                get_updates_buf.clear();
                            }
                            Err(e) => {
                                error!(error = %e, "startup iLink session renewal failed");
                            }
                        }
                    }
                } else {
                    if let Some(ref renewal_ctx) = renewal {
                        set_status(renewal_ctx, crate::hub::ilink_status::CONNECTED);
                    }
                    if let Some(buf) = resp.get_updates_buf {
                        if !buf.is_empty() {
                            get_updates_buf = buf;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "startup session probe failed (network?), continuing");
            }
        }

        loop {
            if *shutdown.borrow() {
                info!("iLink upstream polling shutting down");
                return;
            }

            // Check for manual re-login trigger from admin UI.
            let manual_relogin = if let Some(ref mut r) = renewal {
                if let Some(ref mut rx) = r.relogin_rx {
                    matches!(rx.try_recv(), Ok(()))
                } else {
                    false
                }
            } else {
                false
            };

            if manual_relogin {
                info!("manual re-login triggered from admin UI");
                self.relogin_attempts.fetch_add(1, Ordering::Relaxed);
                if let Some(ref renewal_ctx) = renewal {
                    set_status(renewal_ctx, crate::hub::ilink_status::LOGGING_IN);
                    match renew_expired_session(self.clone(), renewal_ctx).await {
                        Ok(()) => {
                            set_status(renewal_ctx, crate::hub::ilink_status::CONNECTED);
                            get_updates_buf.clear();
                        }
                        Err(e) => {
                            error!(error = %e, "manual iLink session renewal failed");
                            set_status(renewal_ctx, crate::hub::ilink_status::NEEDS_LOGIN);
                        }
                    }
                }
                continue;
            }

            let result = self.get_updates(get_updates_buf.clone(), None).await;

            match result {
                Ok(resp) if resp.ret == Some(0) || resp.errcode.is_none() => {
                    self.polls_ok.fetch_add(1, Ordering::Relaxed);
                    backoff_secs = 1;
                    if let Some(ref renewal_ctx) = renewal {
                        set_status(renewal_ctx, crate::hub::ilink_status::CONNECTED);
                    }
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
                            set_status(renewal_ctx, crate::hub::ilink_status::NEEDS_LOGIN);
                            if renewing
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                self.relogin_attempts.fetch_add(1, Ordering::Relaxed);
                                match renew_expired_session(self.clone(), renewal_ctx).await {
                                    Ok(()) => {
                                        set_status(
                                            renewal_ctx,
                                            crate::hub::ilink_status::CONNECTED,
                                        );
                                        backoff_secs = 1;
                                        get_updates_buf.clear();
                                        renewing.store(false, Ordering::SeqCst);
                                        continue;
                                    }
                                    Err(e) => {
                                        error!(error = %e, "iLink session renewal failed; waiting 30s before retry");
                                        // Fixed wait after renewal failure — prevents tight loop
                                        // hammering the QR endpoint when credentials are broken.
                                        tokio::time::sleep(Duration::from_secs(30)).await;
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

fn set_status(renewal: &SessionRenewal, status: u8) {
    if let Some(ref s) = renewal.ilink_status {
        s.store(status, Ordering::Relaxed);
    }
}

async fn renew_expired_session(
    upstream: Arc<UpstreamClient>,
    renewal: &SessionRenewal,
) -> Result<()> {
    let quiet_ui = renewal.qr_login_ui.is_some() || renewal.qr_tx.is_some();
    if !quiet_ui {
        println!();
        println!("⚠️  iLink 微信登录态已过期，请扫描下方二维码重新登录。");
        println!();
    }
    warn!("iLink session expired (-14), starting QR re-login");

    if let Some(ref s) = renewal.ilink_status {
        s.store(crate::hub::ilink_status::LOGGING_IN, Ordering::Relaxed);
    }

    // Build a combined QR UI sender: prefer desktop channel, fall back to SSE broadcast.
    let ui_tx: Option<UnboundedSender<QrLoginUiEvent>> =
        renewal.qr_login_ui.clone().or_else(|| {
            renewal.qr_tx.as_ref().map(|tx| {
                let (unbounded_tx, mut unbounded_rx) = tokio::sync::mpsc::unbounded_channel();
                let broadcast_tx = tx.clone();
                let last_ready = renewal.qr_last_ready.clone();
                tokio::spawn(async move {
                    while let Some(evt) = unbounded_rx.recv().await {
                        // Cache the Ready event so late SSE subscribers can catch up.
                        if let QrLoginUiEvent::Ready { .. } = &evt {
                            if let Some(ref cache) = last_ready {
                                *cache.lock().await = Some(evt.clone());
                            }
                        } else if matches!(evt, QrLoginUiEvent::Done | QrLoginUiEvent::Expired) {
                            if let Some(ref cache) = last_ready {
                                *cache.lock().await = None;
                            }
                        }
                        let _ = broadcast_tx.send(evt);
                    }
                });
                unbounded_tx
            })
        });

    let login_client = LoginClient::new(renewal.ilink_base_url.clone());
    let token = login_client.login_with_qr_ui(ui_tx).await?;
    let base = renewal
        .ilink_base_url
        .clone()
        .unwrap_or_else(|| ILINK_BASE_URL.to_string());

    renewal.store.save_credentials(&token, &base).await?;
    upstream.set_token(token);
    info!("iLink session renewed, token saved");

    match upstream.notify_start().await {
        Ok(_) => info!("iLink notifystart successful after renewal"),
        Err(e) => {
            warn!(error = %e, "notifystart failed after renewal — outbound messages may not work")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::UpstreamClient;

    #[test]
    fn well_formed_bot_token() {
        assert!(UpstreamClient::is_well_formed_bot_token(
            "bot@im.bot:secret"
        ));
        assert!(!UpstreamClient::is_well_formed_bot_token(""));
        assert!(!UpstreamClient::is_well_formed_bot_token("no-colon"));
    }
}
