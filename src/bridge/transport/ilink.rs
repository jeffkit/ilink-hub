//! iLink (WeChat clawbot) transport adapter.
//!
//! This is the only place in `src/bridge/` that touches `crate::ilink::types`.
//! It wraps the iLink HTTP client ([`HubClient`]) and implements the generic
//! [`Transport`] trait by translating between iLink wire types and the
//! generic DTOs in [`crate::bridge::transport`].
//!
//! `HubClient` talks to the iLink `/ilink/bot/getupdates` and `/sendmessage`
//! endpoints. When pointed at a Hub base URL it speaks the same iLink protocol
//! the Hub relays upstream; when pointed at the real iLink upstream it connects
//! directly (Stage 3 will formalise `via: hub | direct`).

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tracing::warn;

use super::connection::hub_response_token_rejected;
use crate::bridge::transport::{
    InboundMessage, InboundOutcome, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};
use crate::ilink::types::{
    msg_type, BaseInfo, GetUpdatesRequest, GetUpdatesResponse, HubExt, SendMessageRequest,
    SendMessageResponse, WeixinMessage,
};

pub(crate) enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

/// Pure mapping from a parsed `SendMessageResponse` to a [`SendOutcome`].
#[allow(dead_code)]
pub(crate) fn classify_sendoutcome(parsed: Option<&SendMessageResponse>) -> SendOutcome {
    match parsed {
        None => SendOutcome::Sent,
        Some(v) => match v.ret {
            Some(0) => SendOutcome::Sent,
            Some(-2) => SendOutcome::Throttled {
                ret: -2,
                errmsg: v.errmsg.clone(),
            },
            Some(_other) => SendOutcome::Sent,
            None => SendOutcome::Sent,
        },
    }
}

/// Map the raw HTTP response body of `sendmessage` into a [`SendOutcome`].
///
/// Empty bodies are treated as `Sent`. When the body parses as JSON and `ret`
/// is some non-zero value other than -2, this returns `Err` carrying the
/// upstream ret/errmsg. When the body fails to parse entirely, this returns
/// `Ok(Sent)` for backwards compatibility, with the caller logging a warning.
pub(crate) fn parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SendOutcome::Sent);
    }
    match serde_json::from_str::<SendMessageResponse>(trimmed) {
        Ok(v) => {
            let ret = v.ret.unwrap_or(0);
            if ret == -2 {
                Ok(SendOutcome::Throttled {
                    ret: -2,
                    errmsg: v.errmsg,
                })
            } else if ret != 0 {
                Err((ret, v.errmsg))
            } else {
                Ok(SendOutcome::Sent)
            }
        }
        Err(_) => Ok(SendOutcome::Sent),
    }
}

#[derive(Clone)]
pub(crate) struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    pub(crate) fn new(hub_url: String, token: String) -> Result<Self> {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            // Evict idle connections after 30 s. Without this, a connection
            // parked in the pool between two AI calls (which may be minutes
            // apart) can be silently closed by the server-side load balancer
            // or NAT, causing the next `sendmessage` to fail with a transport
            // error and lose the user's reply entirely.
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            http,
            hub_url,
            token,
        })
    }

    pub(crate) async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.clone(),
            base_info: Some(BaseInfo::default()),
            timeout: None,
        };
        let url = format!("{}/ilink/bot/getupdates", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let out: GetUpdatesResponse = resp.json().await?;
        if hub_response_token_rejected(status, out.ret) {
            warn!(
                status = %status,
                errmsg = ?out.errmsg,
                "hub rejected virtual token during getupdates"
            );
            return Ok(GetUpdatesOutcome::TokenRejected);
        }
        if !status.is_success() {
            anyhow::bail!("getupdates HTTP {status}: {:?}", out.errmsg);
        }
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        if out.ret != Some(0) {
            warn!(
                ret = ?out.ret,
                errcode = ?out.errcode,
                errmsg = ?out.errmsg,
                "getupdates returned non-zero ret"
            );
        }
        Ok(GetUpdatesOutcome::Ok(out))
    }

    pub(crate) async fn sendmessage(&self, req: SendMessageRequest) -> Result<SendOutcome> {
        let url = format!("{}/ilink/bot/sendmessage", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendmessage HTTP {status}: {t}");
        }
        let text = resp.text().await?;
        let body_len = text.len();
        match parse_sendoutcome(&text) {
            Ok(out) => {
                if body_len > 0
                    && matches!(out, SendOutcome::Sent)
                    && serde_json::from_str::<SendMessageResponse>(&text).is_err()
                {
                    warn!(
                        body_len,
                        "sendmessage response body failed to parse as JSON; treating as Sent (legacy fallback)"
                    );
                }
                Ok(out)
            }
            Err((other, errmsg)) => {
                anyhow::bail!("sendmessage ret={other} errmsg={:?}", errmsg);
            }
        }
    }
}

/// Build generic [`MediaRef`]s from an iLink message's media items.
///
/// Mirrors the former `executor::build_attachments` shape: under agentproc 0.4
/// all media travels in the turn object's `attachments` field.
fn build_media(msg: &WeixinMessage) -> Vec<MediaRef> {
    let mut out = Vec::new();
    let Some(items) = msg.item_list.as_ref() else {
        return out;
    };
    for item in items.iter() {
        match item.item_type {
            Some(msg_type::IMAGE) => {
                if let Some(url) = item
                    .image_item
                    .as_ref()
                    .and_then(|i| i.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(MediaRef {
                        kind: "image".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            Some(msg_type::FILE) => {
                let file_meta = item.file_item.as_ref().and_then(|fi| {
                    fi.cdn_url
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|url| (url, fi.file_name.as_deref()))
                });
                if let Some((url, fname)) = file_meta {
                    out.push(MediaRef {
                        kind: "file".into(),
                        url: url.to_string(),
                        filename: fname.map(|s| s.to_string()),
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            Some(msg_type::VIDEO) => {
                if let Some(url) = item
                    .video_item
                    .as_ref()
                    .and_then(|v| v.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    out.push(MediaRef {
                        kind: "video".into(),
                        url: url.to_string(),
                        filename: None,
                        mime_type: None,
                        size: None,
                    });
                }
                break;
            }
            _ => {}
        }
    }
    out
}

/// Convert an iLink [`WeixinMessage`] to a generic [`InboundMessage`].
fn weixin_to_inbound(msg: WeixinMessage) -> InboundMessage {
    let context_token = msg.context_token.clone();
    let from_user = msg.from_user_id.clone();
    let is_from_bot = msg.message_type == Some(2);
    let text = msg.text().map(|s| s.to_string());
    let media = build_media(&msg);
    let session_id = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.clone());
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.clone());
    let a2a_call_id = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.a2a_call_id.clone());
    let raw = serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null);
    InboundMessage {
        context_token,
        from_user,
        is_from_bot,
        text,
        media,
        session_id,
        session_name,
        a2a_call_id,
        extra: serde_json::Value::Null,
        raw,
    }
}

/// Convert a generic [`OutboundReply`] to an iLink [`SendMessageRequest`].
fn outbound_to_sendmessage(reply: OutboundReply) -> SendMessageRequest {
    let cli_session_id = reply.cli_session_id.filter(|s| !s.trim().is_empty());
    let mut req = SendMessageRequest::reply_text(
        reply.context_token,
        reply.text,
        &reply.to_user,
        cli_session_id,
    );
    if let Some(ref mut msg) = req.msg {
        let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
        if let Some(sn) = reply.session_name.filter(|s| !s.trim().is_empty()) {
            ext.session_name = Some(sn);
        }
        if let Some(id) = reply.a2a_call_id.filter(|s| !s.trim().is_empty()) {
            ext.a2a_call_id = Some(id);
        }
        if let Some(u) = reply.usage {
            ext.usage = Some(u);
        }
    }
    req
}

/// iLink transport: wraps a [`HubClient`] and speaks the generic [`Transport`]
/// trait. This is the adapter the dispatcher consumes; it hides
/// `crate::ilink::types` from the rest of the bridge.
#[derive(Clone)]
pub struct IlinkTransport {
    client: HubClient,
}

impl IlinkTransport {
    pub fn new(hub_url: String, token: String) -> Result<Self> {
        Ok(Self {
            client: HubClient::new(hub_url, token)?,
        })
    }
}

impl Transport for IlinkTransport {
    fn next_inbound<'a>(&'a self, buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            match self.client.getupdates(buf).await? {
                GetUpdatesOutcome::TokenRejected => Ok(InboundOutcome::TokenRejected),
                GetUpdatesOutcome::Ok(resp) => {
                    let msgs = resp
                        .msgs
                        .unwrap_or_default()
                        .into_iter()
                        .map(weixin_to_inbound)
                        .collect();
                    Ok(InboundOutcome::Messages(msgs))
                }
            }
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        let req = outbound_to_sendmessage(reply);
        Box::pin(async move { self.client.sendmessage(req).await })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_three_categories() {
        let none_resp = SendMessageResponse {
            ret: None,
            errmsg: None,
        };
        let zero_resp = SendMessageResponse {
            ret: Some(0),
            errmsg: None,
        };
        let tmo_resp = SendMessageResponse {
            ret: Some(-2),
            errmsg: Some("rl".into()),
        };
        assert_eq!(classify_sendoutcome(None), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&none_resp)), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&zero_resp)), SendOutcome::Sent);
        assert_eq!(
            classify_sendoutcome(Some(&tmo_resp)),
            SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into())
            }
        );
    }
}
