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
/// Upper bound on a forwarded response body buffered in memory. The local Hub's
/// API responses are small, but cap defensively so a pathological body can't
/// grow this `Vec` without bound. Bodies exceeding the cap are truncated.
const MAX_RELAY_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Maximum time to wait for the next WebSocket frame. If no frame arrives within
/// this window the connection is considered half-open and the client reconnects.
/// The relay server is expected to send at least a ping or a request within this
/// interval; 120 s gives ample room while still recovering from silent hangs.
const WS_IDLE_TIMEOUT_SECS: u64 = 120;

/// RFC 7230 §6.1 hop-by-hop headers that must not be forwarded by a proxy.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Spawn a background task that maintains a relay connection and forwards HTTP to local Hub.
///
/// `relay_secret` is injected as `X-Ilink-Relay-Secret` on every forwarded request so the
/// Hub can distinguish trusted relay-forwarded XFF headers from local-process spoofing.
///
/// Returns when `shutdown` flips to `true`, performing a clean exit so the relay server
/// observes a normal WebSocket close instead of an unexpected drop.
pub fn spawn_relay_client(
    identity: DeviceIdentity,
    hub_base: String,
    relay_ws_url: String,
    relay_secret: String,
    shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(run_relay_loop(
        identity,
        hub_base,
        relay_ws_url,
        relay_secret,
        shutdown,
    ));
}

/// Reconnect loop body, factored out so tests can `await` it directly.
pub async fn run_relay_loop(
    identity: DeviceIdentity,
    hub_base: String,
    relay_ws_url: String,
    relay_secret: String,
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
        // Per-iteration shutdown check: between iterations the receiver may
        // have been notified; cheap to poll again before kicking off I/O.
        if *shutdown.borrow() {
            info!("relay client shutting down (pre-iteration)");
            return;
        }
        let session_shutdown = shutdown.clone();
        // Run the session to completion. `run_session` already races every
        // in-flight I/O against `shutdown` and returns `SessionExit::Shutdown`
        // when it fires, so it must NOT be raced against the reconnect timer
        // here: doing so cancels a *healthy* long-lived session every
        // `RECONNECT_SECS`, producing a register/disconnect flap on the relay
        // (and the resulting reconnect storm trips the relay's rate limiter).
        match run_session(
            &identity,
            &device_id,
            &hub_base,
            &relay_ws_url,
            &relay_secret,
            session_shutdown,
        )
        .await
        {
            Ok(SessionExit::Shutdown) => {
                info!("relay session observed shutdown");
                return;
            }
            Ok(SessionExit::EndedNormally) => info!("relay session ended normally"),
            Err(e) => warn!(error = %e, "relay session error"),
        }

        // Backoff before reconnecting. Interruptible by shutdown so a SIGTERM
        // during the wait exits promptly instead of stalling for the full
        // reconnect interval.
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => {}
            _ = tokio::time::sleep(Duration::from_secs(RECONNECT_SECS)) => {}
        }
        if *shutdown.borrow() {
            info!("relay client shutting down (during reconnect backoff)");
            return;
        }
    }
}

/// Why a `run_session` future returned.
#[derive(Debug, PartialEq, Eq)]
enum SessionExit {
    /// Session exited cleanly without a request triggering shutdown.
    EndedNormally,
    /// Shutdown was observed while the session was active.
    Shutdown,
}

async fn run_session(
    identity: &DeviceIdentity,
    device_id: &str,
    hub_base: &str,
    relay_ws_url: &str,
    relay_secret: &str,
    mut shutdown: watch::Receiver<bool>,
) -> Result<SessionExit> {
    info!(url = %relay_ws_url, device_id = %device_id, "connecting to pairing relay");

    // Race the connect handshake against shutdown so a SIGTERM during a slow
    // TCP handshake does not hold the task open past the kernel's timeout.
    let ws = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => return Ok(SessionExit::Shutdown),
        res = connect_async(relay_ws_url) => match res {
            Ok((ws, _)) => ws,
            Err(e) => anyhow::bail!("connect {relay_ws_url}: {e}"),
        },
    };
    let (mut write, mut read) = ws.split();

    let timestamp = unix_now();
    let register = RelayMessage::Register {
        device_id: device_id.to_string(),
        public_key: identity.public_key_b64()?,
        timestamp,
        signature: identity.sign_register(timestamp)?,
    };
    // Same race for the register write — if shutdown lands while the write is
    // queued, drop the future rather than block on the WebSocket send.
    let send_fut = write.send(Message::Text(register.to_json()?.into()));
    tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => return Ok(SessionExit::Shutdown),
        res = send_fut => res?,
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()?;

    loop {
        // Read the next message or react to shutdown. A per-frame idle timeout
        // (WS_IDLE_TIMEOUT_SECS) ensures half-open connections are detected and
        // the client reconnects rather than hanging silently (TO-03).
        let frame_result = tokio::time::timeout(Duration::from_secs(WS_IDLE_TIMEOUT_SECS), async {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown) => None,
                msg = read.next() => Some(msg),
            }
        })
        .await;

        let msg = match frame_result {
            Err(_) => {
                warn!(
                    ws_idle_timeout_secs = WS_IDLE_TIMEOUT_SECS,
                    "relay WebSocket idle timeout; reconnecting"
                );
                return Ok(SessionExit::EndedNormally);
            }
            Ok(None) => return Ok(SessionExit::Shutdown),
            Ok(Some(Some(Ok(m)))) => m,
            Ok(Some(Some(Err(e)))) => return Err(e.into()),
            Ok(Some(None)) => return Ok(SessionExit::EndedNormally),
        };

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
                    let forward_fut = forward_to_hub(
                        &http,
                        hub_base,
                        &method,
                        &path,
                        &headers,
                        body.as_deref(),
                        relay_secret,
                    );
                    let res = tokio::select! {
                        biased;
                        _ = wait_for_shutdown(&mut shutdown) => return Ok(SessionExit::Shutdown),
                        res = forward_fut => res,
                    };
                    match res {
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
                            body: Some(serde_json::json!({"error": e.to_string()}).to_string()),
                        },
                    }
                };
                // Race the reply write against shutdown too: an in-flight reply
                // must not hold the task open past a SIGTERM.
                let send_fut = write.send(Message::Text(reply.to_json()?.into()));
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => return Ok(SessionExit::Shutdown),
                    res = send_fut => res?,
                }
            }
            RelayMessage::Ping => {
                let send_fut = write.send(Message::Text(RelayMessage::Pong.to_json()?.into()));
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => return Ok(SessionExit::Shutdown),
                    res = send_fut => res?,
                }
            }
            _ => {}
        }
    }
}

/// Resolve when `shutdown` flips to `true`. Used as one arm of a `select!`
/// race so that an in-flight I/O future can be dropped the moment shutdown
/// is signalled, regardless of how long the I/O would otherwise take.
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    // Already signalled — return immediately without consuming a notification.
    if *shutdown.borrow() {
        return;
    }
    // Wait for the next change. We don't care *what* it changed to here — the
    // caller will re-check the borrow() after this resolves if it needs to
    // distinguish "shutdown" from "spurious change".
    let _ = shutdown.changed().await;
}

async fn forward_to_hub(
    http: &reqwest::Client,
    hub_base: &str,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    relay_secret: &str,
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
        if is_blocked_forward_header(k) {
            continue;
        }
        req = req.header(k, v);
    }
    // Prove to the Hub that this forwarded request came from the in-process relay
    // client (not an arbitrary local process). The Hub trusts X-Forwarded-For only
    // when this secret matches.
    req = req.header("x-ilink-relay-secret", relay_secret);

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
    let body_text = read_body_capped(resp, MAX_RELAY_BODY_BYTES).await;

    Ok((status, resp_headers, body_text))
}

/// Read a response body into a `String`, stopping after `cap` bytes so a
/// pathologically large body cannot exhaust memory.
///
/// Mirrors the prior `resp.text().await.ok()` semantics: a clean (even empty)
/// body yields `Some(_)`, while a read error yields `None`. Bodies past `cap`
/// are truncated and logged.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let remaining = cap.saturating_sub(buf.len());
                let take = remaining.min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    warn!(
                        cap_bytes = cap,
                        "relay response body exceeded cap; truncating"
                    );
                    break;
                }
            }
            // Mirror `resp.text()`: any read error maps the whole body to None.
            Some(Err(_)) => return None,
            None => break,
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `true` if `name` matches a header that must never be forwarded from the
/// untrusted relay side to the local hub. Covers the full RFC 7230 §6.1
/// hop-by-hop set plus `host` and `content-length` which would otherwise be
/// re-derived by reqwest from the request URL.
fn is_blocked_forward_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "host" || lower == "content-length" {
        return true;
    }
    HOP_BY_HOP_HEADERS.iter().any(|h| *h == lower)
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
    use rand_core::OsRng;
    use tokio::sync::watch;

    fn test_identity() -> DeviceIdentity {
        let signing_key = SigningKey::generate(&mut OsRng);
        DeviceIdentity::for_testing(
            "550e8400-e29b-41d4-a716-446655440000".to_string(),
            B64.encode(signing_key.to_bytes()),
        )
    }

    /// Bind a TcpListener on an OS-assigned port, then drop it so the kernel
    /// holds a brief TIME_WAIT / refused state on that port. Using port 0 is
    /// more portable than `127.0.0.1:1` because it cannot collide with an
    /// unrelated process that happens to bind low ports in a sandbox.
    async fn refused_addr() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        addr.to_string()
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
                String::new(),
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
        let unreachable = refused_addr().await;
        let (tx, rx) = watch::channel(false);
        let identity = test_identity();
        let loop_handle = tokio::spawn(run_relay_loop(
            identity,
            format!("http://{unreachable}"),
            format!("ws://{unreachable}/ws/pairing"),
            String::new(),
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
            String::new(),
            rx,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "spawn_relay_client blocked for {elapsed:?} (expected <50ms)"
        );
    }

    // ── Adversarial tests for F-1: shutdown must interrupt every in-flight
    //    I/O inside `run_session`, not just the outer reconnect sleep. ─────

    /// Bind a TcpListener that accepts the TCP connection but never completes
    /// the WebSocket upgrade — i.e. `connect_async` is mid-handshake. The
    /// shutdown signal must still drop the task within a small bound, even
    /// though the handshake would otherwise hang for the OS TCP timeout.
    #[tokio::test]
    async fn relay_loop_exits_during_held_handshake_when_shutdown_signalled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local_addr");

        // Hold the half-open connections open so `connect_async` never
        // returns. We drop the listener only after the test asserts exit;
        // otherwise the kernel RSTs the new SYN and the test no longer
        // exercises the handshake-stuck code path.
        let hold = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                // Keep the socket open by parking it in a task that
                // never reads or writes. Drop happens when the test
                // ends and the outer task is cancelled.
                tokio::spawn(async move {
                    let _hold = sock;
                    std::future::pending::<()>().await;
                });
            }
        });

        let (tx, rx) = watch::channel(false);
        let identity = test_identity();
        let loop_handle = tokio::spawn(run_relay_loop(
            identity,
            format!("http://{addr}"),
            format!("ws://{addr}/ws/pairing"),
            String::new(),
            rx,
        ));

        // Give the loop time to enter connect_async and block on the
        // handshake. connect_async sends the HTTP Upgrade request and waits
        // for the 101 Switching Protocols response; until our accept loop
        // responds, that future never resolves.
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(true).expect("send shutdown");

        match tokio::time::timeout(Duration::from_secs(1), loop_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("relay loop task panicked: {e}"),
            Err(_) => panic!(
                "relay loop did not exit within 1s of shutdown while handshake was held — \
                 the in-flight connect_async is not interruptible"
            ),
        }

        hold.abort();
        let _ = hold.await;
    }

    /// Header pass-through must strip both the simple blocklist (host,
    /// content-length) and the full RFC 7230 hop-by-hop set. This is
    /// exercised indirectly by `forward_to_hub`; the unit test pins the
    /// predicate so a future refactor cannot silently drop one entry.
    #[test]
    fn hop_by_hop_header_predicate_blocks_all_rfc7230_hop_by_hop() {
        for h in HOP_BY_HOP_HEADERS {
            assert!(
                is_blocked_forward_header(h),
                "hop-by-hop header {h:?} must be blocked"
            );
            assert!(
                is_blocked_forward_header(&h.to_uppercase()),
                "hop-by-hop header {h:?} must be blocked (case-insensitive)"
            );
        }
        assert!(is_blocked_forward_header("host"));
        assert!(is_blocked_forward_header("Host"));
        assert!(is_blocked_forward_header("content-length"));
        assert!(is_blocked_forward_header("Content-Length"));

        // Headers that should still pass through.
        assert!(!is_blocked_forward_header("authorization"));
        assert!(!is_blocked_forward_header("x-custom-header"));
        assert!(!is_blocked_forward_header("user-agent"));
    }

    /// Test that when the relay client receives a request message and starts
    /// forwarding it to the hub (which takes a long time/hangs), the shutdown
    /// signal still interrupts the task immediately.
    #[tokio::test]
    async fn relay_loop_exits_during_http_forward_when_shutdown_signalled() {
        // 1. Mock WebSocket server for the relay client to connect to
        let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ws listener");
        let ws_addr = ws_listener.local_addr().expect("local_addr");

        let mock_ws = tokio::spawn(async move {
            if let Ok((stream, _)) = ws_listener.accept().await {
                if let Ok(mut ws_stream) = tokio_tungstenite::accept_async(stream).await {
                    // Read Register message
                    if let Some(Ok(Message::Text(_))) = ws_stream.next().await {
                        // Send Request message to trigger forward_to_hub
                        let req = RelayMessage::Request {
                            id: "req-1".to_string(),
                            method: "GET".to_string(),
                            path: "/hub/pair/abc".to_string(),
                            headers: HashMap::new(),
                            body: None,
                        };
                        let _ = ws_stream
                            .send(Message::Text(req.to_json().unwrap().into()))
                            .await;

                        // Keep connection open
                        std::future::pending::<()>().await;
                    }
                }
            }
        });

        // 2. Mock Hub HTTP server that accepts connection but hangs
        let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hub listener");
        let hub_addr = hub_listener.local_addr().expect("local_addr");

        let mock_hub = tokio::spawn(async move {
            if let Ok((mut stream, _)) = hub_listener.accept().await {
                // Read request headers to clear the buffer
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                // Hang forever to simulate slow hub
                std::future::pending::<()>().await;
            }
        });

        let (tx, rx) = watch::channel(false);
        let identity = test_identity();
        let loop_handle = tokio::spawn(run_relay_loop(
            identity,
            format!("http://{hub_addr}"),
            format!("ws://{ws_addr}/ws/pairing"),
            String::new(),
            rx,
        ));

        // Give the loop time to connect, register, receive request, and enter forward_to_hub
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Signal shutdown
        tx.send(true).expect("send shutdown");

        // The loop must abort mid-forwarding and exit quickly.
        match tokio::time::timeout(Duration::from_secs(2), loop_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("relay loop task panicked: {e}"),
            Err(_) => panic!(
                "relay loop did not exit within 2s of shutdown while HTTP forwarding was stuck"
            ),
        }

        mock_ws.abort();
        mock_hub.abort();
        let _ = mock_ws.await;
        let _ = mock_hub.await;
    }

    /// `unix_now()` must return a positive Unix timestamp in a plausible range.
    /// This pins the `as_secs() as i64` conversion so a mutant swapping the cast
    /// type (e.g. `as u64`) or changing the epoch origin is immediately caught.
    #[test]
    fn unix_now_returns_positive_timestamp_in_plausible_range() {
        let ts = unix_now();
        // 2020-01-01 00:00:00 UTC in Unix seconds
        let year_2020: i64 = 1_577_836_800;
        // 2100-01-01 00:00:00 UTC in Unix seconds
        let year_2100: i64 = 4_102_444_800;
        assert!(
            ts > year_2020 && ts < year_2100,
            "unix_now() = {ts} is outside the expected [2020, 2100) range"
        );
    }
}
