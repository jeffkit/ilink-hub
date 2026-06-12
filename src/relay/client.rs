//! Outbound WebSocket client — connects Hub to the public pairing relay.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::device::{is_allowed_relay_path, DeviceIdentity};
use super::protocol::RelayMessage;

const RECONNECT_SECS: u64 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Spawn a background task that maintains a relay connection and forwards HTTP to local Hub.
///
/// Returns when `shutdown` flips to `true`, performing a clean exit so the relay server
/// observes a normal WebSocket close instead of an unexpected drop.
pub fn spawn_relay_client(
    identity: DeviceIdentity,
    hub_base: String,
    relay_ws_url: String,
    shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(run_relay_loop(identity, hub_base, relay_ws_url, shutdown));
}

/// Reconnect loop body, factored out so tests can `await` it directly.
pub async fn run_relay_loop(
    identity: DeviceIdentity,
    hub_base: String,
    relay_ws_url: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let device_id = identity.device_id().to_string();
    // If shutdown is already true at startup (e.g. shutdown raced with startup
    // ordering), exit immediately rather than connecting once and dropping the
    // socket on the way out.
    if *shutdown.borrow() {
        info!("relay client skipped (shutdown already signalled at spawn)");
        return;
    }
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("relay client shutting down");
                    return;
                }
            }
            res = run_session(&identity, &device_id, &hub_base, &relay_ws_url) => {
                match res {
                    Ok(()) => info!("relay session ended normally"),
                    Err(e) => warn!(error = %e, "relay session error"),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(RECONNECT_SECS)) => {}
        }
    }
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
                    match forward_to_hub(&http, hub_base, &method, &path, &headers, body.as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use tokio::sync::watch;

    fn test_identity() -> DeviceIdentity {
        let signing_key = SigningKey::generate(&mut OsRng);
        DeviceIdentity::for_testing(
            "550e8400-e29b-41d4-a716-446655440000".to_string(),
            B64.encode(signing_key.to_bytes()),
        )
    }

    /// If `shutdown` is already true at spawn time, the loop must exit immediately
    /// without ever calling `run_session` (and therefore without opening any WebSocket).
    #[tokio::test]
    async fn relay_loop_returns_immediately_when_shutdown_already_true() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).expect("send shutdown");
        let identity = test_identity();
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            run_relay_loop(
                identity,
                "http://127.0.0.1:1".to_string(),
                "ws://127.0.0.1:1/ws/pairing".to_string(),
                rx,
            ),
        )
        .await;
        assert!(
            result.is_ok(),
            "run_relay_loop did not return within 200ms when shutdown was signalled at spawn"
        );
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "loop took {}ms to exit; expected near-instant",
            start.elapsed().as_millis()
        );
    }

    /// When `run_session` fails (e.g. unreachable relay URL) and the loop falls
    /// into the `RECONNECT_SECS` sleep, sending `shutdown` must abort the sleep
    /// and return promptly — well under the 5s reconnect interval.
    #[tokio::test]
    async fn relay_loop_exits_during_reconnect_sleep_when_shutdown_signalled() {
        let (tx, rx) = watch::channel(false);
        let identity = test_identity();
        let loop_handle = tokio::spawn(run_relay_loop(
            identity,
            "http://127.0.0.1:1".to_string(),
            // Unreachable WS URL — connect_async fails fast on 127.0.0.1:1.
            "ws://127.0.0.1:1/ws/pairing".to_string(),
            rx,
        ));

        // Give the loop time to attempt connect_async and fall into the
        // 5s reconnect sleep.
        tokio::time::sleep(Duration::from_millis(500)).await;
        tx.send(true).expect("send shutdown");

        // The sleep arm must abort via the select!'s `shutdown.changed()` arm.
        // Allow a generous bound (RECONNECT_SECS is 5s, so anything <2s proves
        // shutdown was honoured).
        match tokio::time::timeout(Duration::from_secs(2), loop_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("relay loop task panicked: {e}"),
            Err(_) => panic!("relay loop did not exit within 2s of shutdown signal"),
        }
    }

    /// `spawn_relay_client` must return synchronously (it just spawns a task) so the
    /// caller's startup sequence is not blocked by relay connectivity.
    #[tokio::test]
    async fn spawn_relay_client_returns_without_blocking() {
        let identity = test_identity();
        let (_tx, rx) = watch::channel(false);
        let start = std::time::Instant::now();
        spawn_relay_client(
            identity,
            "http://127.0.0.1:1".to_string(),
            "ws://127.0.0.1:1/ws/pairing".to_string(),
            rx,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "spawn_relay_client blocked for {elapsed:?} (expected <50ms)"
        );
    }
}
