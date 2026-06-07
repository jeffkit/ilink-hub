//! Outbound WebSocket client — connects Hub to the public pairing relay.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::device::{is_allowed_relay_path, DeviceIdentity};
use super::protocol::RelayMessage;

const RECONNECT_SECS: u64 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Spawn a background task that maintains a relay connection and forwards HTTP to local Hub.
pub fn spawn_relay_client(identity: DeviceIdentity, hub_base: String, relay_ws_url: String) {
    let device_id = identity.device_id().to_string();
    tokio::spawn(async move {
        loop {
            match run_session(&identity, &device_id, &hub_base, &relay_ws_url).await {
                Ok(()) => info!("relay session ended normally"),
                Err(e) => warn!(error = %e, "relay session error"),
            }
            tokio::time::sleep(Duration::from_secs(RECONNECT_SECS)).await;
        }
    });
}

async fn run_session(
    identity: &DeviceIdentity,
    device_id: &str,
    hub_base: &str,
    relay_ws_url: &str,
) -> Result<()> {
    info!(url = %relay_ws_url, device_id = %device_id, "connecting to pairing relay");

    let (ws, _) = connect_async(relay_ws_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect {relay_ws_url}: {e}"))?;
    let (mut write, mut read) = ws.split();

    let timestamp = unix_now();
    let register = RelayMessage::Register {
        device_id: device_id.to_string(),
        public_key: identity.public_key_b64()?,
        timestamp,
        signature: identity.sign_register(timestamp)?,
    };
    write
        .send(Message::Text(register.to_json()?.into()))
        .await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()?;

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if !msg.is_text() {
            continue;
        }
        let text = msg.to_text()?;
        let parsed = match RelayMessage::from_json(text) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "invalid relay message");
                continue;
            }
        };

        match parsed {
            RelayMessage::Registered { ok, error } => {
                if ok {
                    info!(device_id = %device_id, "registered with pairing relay");
                } else {
                    anyhow::bail!(
                        "relay registration failed: {}",
                        error.unwrap_or_else(|| "unknown".into())
                    );
                }
            }
            RelayMessage::Request {
                id,
                method,
                path,
                headers,
                body,
            } => {
                debug!(request_id = %id, %method, %path, "relay request");
                let reply = if !is_allowed_relay_path(&path) {
                    warn!(%path, "rejected relay request outside pairing whitelist");
                    RelayMessage::Response {
                        id,
                        status: 403,
                        headers: HashMap::new(),
                        body: Some(r#"{"error":"forbidden path"}"#.into()),
                    }
                } else {
                    match forward_to_hub(
                        &http,
                        hub_base,
                        &method,
                        &path,
                        &headers,
                        body.as_deref(),
                    )
                    .await
                    {
                        Ok((status, headers, body)) => RelayMessage::Response {
                            id,
                            status,
                            headers,
                            body,
                        },
                        Err(e) => RelayMessage::Response {
                            id,
                            status: 502,
                            headers: HashMap::new(),
                            body: Some(format!("{{\"error\":\"{e}\"}}")),
                        },
                    }
                };
                write.send(Message::Text(reply.to_json()?.into())).await?;
            }
            RelayMessage::Ping => {
                write
                    .send(Message::Text(RelayMessage::Pong.to_json()?.into()))
                    .await?;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn forward_to_hub(
    http: &reqwest::Client,
    hub_base: &str,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
) -> Result<(u16, HashMap<String, String>, Option<String>)> {
    let url = format!("{hub_base}{path}");
    let mut req = match method.to_uppercase().as_str() {
        "GET" => http.get(&url),
        "POST" => http.post(&url),
        "PUT" => http.put(&url),
        "DELETE" => http.delete(&url),
        "PATCH" => http.patch(&url),
        "HEAD" => http.head(&url),
        other => anyhow::bail!("unsupported method: {other}"),
    };

    for (k, v) in headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        req = req.header(k, v);
    }

    if let Some(b) = body {
        req = req.body(b.to_string());
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let mut resp_headers = HashMap::new();
    for (k, v) in resp.headers() {
        if let Ok(s) = v.to_str() {
            resp_headers.insert(k.to_string(), s.to_string());
        }
    }
    let body_text = resp.text().await.ok();

    Ok((status, resp_headers, body_text))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}
