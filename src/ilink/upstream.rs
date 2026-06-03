/// Upstream iLink client — connects to the real `ilinkai.weixin.qq.com`
/// and fans received messages out to the Hub's internal message bus.

use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use reqwest::Client;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::types::*;

pub struct UpstreamClient {
    client: Client,
    base_url: String,
    token: String,
    // Random base64 UIN required by iLink (regenerated per request)
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
            token,
        }
    }

    fn random_uin(&self) -> String {
        use rand::Rng;
        use base64::Engine;
        let bytes: [u8; 16] = rand::thread_rng().gen();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
        );
        headers.insert(
            "X-WECHAT-UIN",
            HeaderValue::from_str(&self.random_uin()).unwrap(),
        );
        headers
    }

    /// Long-poll for new messages. Returns when messages arrive or after `timeout` seconds.
    pub async fn get_updates(
        &self,
        buf: Option<String>,
        timeout: u32,
    ) -> Result<GetUpdatesResponse> {
        let url = format!("{}/ilink/bot/getupdates", self.base_url);
        let req = GetUpdatesRequest {
            buf,
            timeout: Some(timeout),
        };
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?
            .json::<GetUpdatesResponse>()
            .await?;
        Ok(resp)
    }

    pub async fn send_message(&self, req: SendMessageRequest) -> Result<SendMessageResponse> {
        let url = format!("{}/ilink/bot/sendmessage", self.base_url);
        let resp = self
            .client
            .post(&url)
            .headers(self.headers())
            .json(&req)
            .send()
            .await?
            .json::<SendMessageResponse>()
            .await?;
        Ok(resp)
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
    /// Exits when `shutdown` is triggered.
    pub async fn run_polling_loop(
        self: Arc<Self>,
        tx: broadcast::Sender<InboundMessage>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut buf: Option<String> = None;
        let mut backoff_secs = 1u64;

        info!("iLink upstream polling started");

        loop {
            if *shutdown.borrow() {
                info!("iLink upstream polling shutting down");
                return;
            }

            let result = self.get_updates(buf.clone(), 30).await;

            match result {
                Ok(resp) if resp.ret == 0 => {
                    backoff_secs = 1;
                    buf = resp.buf;
                    if let Some(messages) = resp.list {
                        for msg in messages {
                            debug!(msg_id = %msg.msg_id, "received upstream message");
                            let _ = tx.send(msg);
                        }
                    }
                }
                Ok(resp) => {
                    warn!(ret = resp.ret, errmsg = ?resp.errmsg, "iLink upstream returned error");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                }
                Err(e) => {
                    error!(error = %e, "iLink upstream request failed");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                }
            }
        }
    }
}
