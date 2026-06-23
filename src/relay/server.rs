//! Public pairing relay server — forwards phone HTTP to connected Hubs via WebSocket.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::{
    body::Body,
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{oneshot, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

use super::auth::{verify_register, verifying_key_from_b64};
use super::device::validate_device_id;
use super::protocol::RelayMessage;
use super::ratelimit::RateLimiter;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Server sends a WebSocket Ping every 60 s to keep the connection alive.
/// Hub relay clients disconnect after 120 s of inactivity; pinging at half
/// that interval gives a 2× safety margin even under occasional packet loss.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
/// Max requests buffered toward a single connected Hub before we shed load.
/// Each entry is an in-flight request awaiting a `REQUEST_TIMEOUT` response, so
/// this bounds memory if a Hub's WebSocket write stalls; excess requests get a
/// 503 instead of growing an unbounded queue.
const HUB_OUTBOUND_CAPACITY: usize = 256;

/// Maximum number of distinct Hubs (i.e. distinct `device_id`s) the relay
/// will register at once. Prevents a misconfigured or malicious Hub
/// fleet from exhaust­ing the relay's `device_keys` map, the `hubs` map,
/// and the `pending` request map. With a 32-byte device-key per Hub, 1024
/// Hubs cost ~32 KiB of memory; raising the cap above 4096 is rarely
/// justified.
const MAX_REGISTERED_HUBS: usize = 1024;

// Per-IP: WS handshakes (Hub reconnects every ~5s → need headroom); pairing HTTP from phones.
const WS_RATE_MAX: usize = 120;
const WS_RATE_WINDOW_SECS: u64 = 600;
const PAIR_RATE_MAX: usize = 30;
const PAIR_RATE_WINDOW_SECS: u64 = 60;

#[derive(Clone)]
pub struct RelayState {
    hubs: Arc<RwLock<HashMap<String, HubHandle>>>,
    pending: Arc<DashMap<String, oneshot::Sender<RelayMessage>>>,
    device_keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
    ws_rate_limiter: Arc<RateLimiter>,
    pair_rate_limiter: Arc<RateLimiter>,
    /// Tracks (device_id, timestamp_secs) pairs that have already been used for
    /// registration. Prevents replay attacks within the 60-second skew window.
    /// Entries are passively evicted whenever a new registration arrives.
    /// Only prevents replay within a single relay process (single-instance relay).
    used_register_nonces: Arc<DashMap<(String, i64), Instant>>,
}

/// Monotonically increasing counter used to generate per-connection IDs.
/// Ordering::Relaxed is sufficient — we only need uniqueness, not ordering
/// across threads.
static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone)]
struct HubHandle {
    tx: tokio::sync::mpsc::Sender<RelayMessage>,
    /// Unique identifier assigned when this connection was registered.
    /// Used to detect the reconnect race: if a new connection has already
    /// replaced this entry in `hubs`, the old connection's cleanup must
    /// NOT evict the new registration.
    conn_id: u64,
}

pub fn build_relay_router() -> Router {
    let state = RelayState {
        hubs: Arc::new(RwLock::new(HashMap::new())),
        pending: Arc::new(DashMap::new()),
        device_keys: Arc::new(RwLock::new(HashMap::new())),
        ws_rate_limiter: Arc::new(RateLimiter::new(WS_RATE_MAX, WS_RATE_WINDOW_SECS)),
        pair_rate_limiter: Arc::new(RateLimiter::new(PAIR_RATE_MAX, PAIR_RATE_WINDOW_SECS)),
        used_register_nonces: Arc::new(DashMap::new()),
    };

    Router::new()
        .route("/health", get(health))
        .route("/ws/pairing", get(ws_pairing))
        .route("/pair/{device_id}/{code}", get(pair_page))
        .route(
            "/pair/{device_id}/{code}/confirm",
            axum::routing::post(pair_confirm),
        )
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        r#"{"status":"healthy","service":"ilink-relay"}"#,
    )
}

async fn ws_pairing(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    if !state.ws_rate_limiter.allow(&addr.ip().to_string()) {
        warn!(ip = %addr.ip(), "ws pairing rate limited");
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    ws.on_upgrade(move |socket| handle_hub_socket(state, socket))
}

async fn accept_registration(
    state: &RelayState,
    device_id: &str,
    public_key: &str,
    timestamp: i64,
    signature: &str,
) -> Result<(), String> {
    if !validate_device_id(device_id) {
        return Err("invalid device_id".into());
    }

    let verifying_key = verifying_key_from_b64(public_key).map_err(|e| e.to_string())?;
    verify_register(&verifying_key, device_id, timestamp, signature, unix_now())
        .map_err(|e| e.to_string())?;

    // Replay-attack prevention: reject if this (device_id, timestamp) pair has
    // already been used within the 60-second skew window.
    let nonce_key = (device_id.to_string(), timestamp);
    // Passively evict expired nonces (older than the max skew window) before checking.
    let skew = Duration::from_secs(super::auth::REGISTER_MAX_SKEW_SECS as u64);
    state
        .used_register_nonces
        .retain(|_, seen_at| seen_at.elapsed() < skew);
    if state.used_register_nonces.contains_key(&nonce_key) {
        return Err("registration nonce already used; replay detected".into());
    }
    state.used_register_nonces.insert(nonce_key, Instant::now());

    let key_bytes = verifying_key.to_bytes();
    let mut keys = state.device_keys.write().await;
    if let Some(existing) = keys.get(device_id) {
        if existing != &key_bytes {
            return Err("device_id already bound to another key".into());
        }
    } else {
        keys.insert(device_id.to_string(), key_bytes);
    }
    Ok(())
}

async fn handle_hub_socket(state: RelayState, socket: WebSocket) {
    let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (mut write, mut read) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<RelayMessage>(HUB_OUTBOUND_CAPACITY);
    let mut device_id: Option<String> = None;

    let register_deadline = tokio::time::sleep(REGISTER_TIMEOUT);
    tokio::pin!(register_deadline);

    // Send a WebSocket Ping every KEEPALIVE_INTERVAL once the hub has registered.
    // This prevents the hub client's idle-timeout from firing when there are no
    // active pairing requests — without pings the client disconnects after 120 s
    // and spends 5-38 s reconnecting, during which pairing URLs return 503.
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    // Skip the immediate first tick so we don't ping before the hub has had a
    // chance to send the Register message (REGISTER_TIMEOUT window is 15 s, well
    // within the 60 s interval).
    keepalive.tick().await;

    loop {
        tokio::select! {
            _ = &mut register_deadline, if device_id.is_none() => {
                warn!("hub registration timeout");
                break;
            }
            _ = keepalive.tick(), if device_id.is_some() => {
                if write.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            incoming = read.next() => {
                let Some(msg) = incoming else { break };
                let Ok(msg) = msg else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(parsed) = RelayMessage::from_json(text.as_ref()) else { continue };

                match parsed {
                    RelayMessage::Register {
                        device_id: id,
                        public_key,
                        timestamp,
                        signature,
                    } if device_id.is_none() => {
                        // Enforce the global Hub cap BEFORE running the
                        // Ed25519 verification (which is the expensive part).
                        // Re-registration of an already-registered Hub is
                        // exempt — the existing handle is replaced in place
                        // (a Hub restart is the common case).
                        {
                            let hubs = state.hubs.read().await;
                            if !hubs.contains_key(&id) && hubs.len() >= MAX_REGISTERED_HUBS {
                                let reason = format!(
                                    "relay at capacity ({MAX_REGISTERED_HUBS} Hubs); \
                                     retry later"
                                );
                                warn!(device_id = %id, "hub registration rejected: cap reached");
                                let err = RelayMessage::Registered {
                                    ok: false,
                                    error: Some(reason),
                                };
                                let _ = write
                                    .send(Message::Text(err.to_json().unwrap_or_default().into()))
                                    .await;
                                break;
                            }
                        }
                        match accept_registration(&state, &id, &public_key, timestamp, &signature).await {
                            Ok(()) => {
                                state.hubs.write().await.insert(
                                    id.clone(),
                                    HubHandle { tx: out_tx.clone(), conn_id },
                                );
                                device_id = Some(id.clone());
                                info!(device_id = %id, "hub registered");
                                let ok = RelayMessage::Registered { ok: true, error: None };
                                let _ = write.send(Message::Text(ok.to_json().unwrap_or_default().into())).await;
                            }
                            Err(reason) => {
                                warn!(device_id = %id, reason = %reason, "hub registration rejected");
                                let err = RelayMessage::Registered {
                                    ok: false,
                                    error: Some(reason),
                                };
                                let _ = write.send(Message::Text(err.to_json().unwrap_or_default().into())).await;
                                break;
                            }
                        }
                    }
                    RelayMessage::Response { id, status, headers, body } => {
                        let tx = state.pending.remove(&id).map(|(_, tx)| tx);
                        if let Some(tx) = tx {
                            let _ = tx.send(RelayMessage::Response { id, status, headers, body });
                        }
                    }
                    _ => {}
                }
            }
            Some(outgoing) = out_rx.recv() => {
                if let Ok(json) = outgoing.to_json() {
                    if write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    if let Some(id) = device_id {
        let mut hubs = state.hubs.write().await;
        // Only evict the hub entry when it still belongs to *this* connection.
        // If the same device_id reconnected while this connection was winding
        // down, the new connection has already inserted a fresh entry with a
        // different conn_id; removing it would cause an intermittent 503.
        if hubs.get(&id).is_some_and(|h| h.conn_id == conn_id) {
            hubs.remove(&id);
        }
        info!(device_id = %id, conn_id, "hub disconnected");
    }
}

struct PendingRequestGuard {
    pending: Arc<DashMap<String, oneshot::Sender<RelayMessage>>>,
    request_id: String,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.pending.remove(&self.request_id);
    }
}

async fn forward_to_hub(
    state: &RelayState,
    device_id: &str,
    method: &str,
    path: &str,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<RelayMessage, StatusCode> {
    let hub = {
        let hubs = state.hubs.read().await;
        hubs.get(device_id).cloned()
    };

    let hub = hub.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let request_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();

    state.pending.insert(request_id.clone(), tx);

    let _guard = PendingRequestGuard {
        pending: state.pending.clone(),
        request_id: request_id.clone(),
    };

    let mut hdr_map = HashMap::new();
    for (k, v) in headers.iter() {
        if let Ok(s) = v.to_str() {
            hdr_map.insert(k.to_string(), s.to_string());
        }
    }

    let req = RelayMessage::Request {
        id: request_id.clone(),
        method: method.to_string(),
        path: path.to_string(),
        headers: hdr_map,
        body,
    };

    // Non-blocking send: if the Hub's outbound queue is full (slow WebSocket) or
    // closed, shed load with 503 rather than buffering without bound.
    if hub.tx.try_send(req).is_err() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
        Ok(Ok(resp)) => Ok(resp),
        _ => Err(StatusCode::GATEWAY_TIMEOUT),
    }
}

fn relay_response_to_http(resp: RelayMessage) -> Response {
    let RelayMessage::Response {
        status,
        headers,
        body,
        ..
    } = resp
    else {
        return (StatusCode::BAD_GATEWAY, "invalid relay response").into_response();
    };

    let mut builder = Response::builder().status(status);
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        builder = builder.header(k, v);
    }
    let body = body.unwrap_or_default();
    builder.body(Body::from(body)).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "response build error").into_response()
    })
}

fn check_pair_rate(state: &RelayState, addr: &SocketAddr) -> Result<(), StatusCode> {
    if state.pair_rate_limiter.allow(&addr.ip().to_string()) {
        Ok(())
    } else {
        warn!(ip = %addr.ip(), "pairing HTTP rate limited");
        Err(StatusCode::TOO_MANY_REQUESTS)
    }
}

async fn pair_page(
    State(state): State<RelayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path((device_id, code)): Path<(String, String)>,
) -> Response {
    if let Err(status) = check_pair_rate(&state, &addr) {
        return (status, "too many pairing requests").into_response();
    }

    match forward_to_hub(
        &state,
        &device_id,
        "GET",
        &format!("/hub/pair/{code}"),
        HeaderMap::new(),
        None,
    )
    .await
    {
        Ok(resp) => relay_response_to_http(resp),
        Err(status) => (
            status,
            format!("Hub 未在线，请确认本机 ilink-hub serve 正在运行 (device: {device_id})"),
        )
            .into_response(),
    }
}

async fn pair_confirm(
    State(state): State<RelayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path((device_id, code)): Path<(String, String)>,
    mut headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(status) = check_pair_rate(&state, &addr) {
        return (status, "too many pairing requests").into_response();
    }

    // Inject the real phone IP so the Hub rate-limiter keys on the phone,
    // not the relay's loopback address. The Hub only trusts this from loopback.
    if let Ok(v) = addr.ip().to_string().parse() {
        headers.insert("x-forwarded-for", v);
    }

    match forward_to_hub(
        &state,
        &device_id,
        "POST",
        &format!("/hub/pair/{code}/confirm"),
        headers,
        Some(body),
    )
    .await
    {
        Ok(resp) => relay_response_to_http(resp),
        Err(status) => (
            status,
            format!("Hub 未在线，请确认本机 ilink-hub serve 正在运行 (device: {device_id})"),
        )
            .into_response(),
    }
}

pub async fn serve(addr: &str) -> Result<()> {
    let router = build_relay_router();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "ilink-relay listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_relay_pending_request_no_leak_on_cancel() {
        let state = RelayState {
            hubs: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(DashMap::new()),
            device_keys: Arc::new(RwLock::new(HashMap::new())),
            ws_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            pair_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            used_register_nonces: Arc::new(DashMap::new()),
        };

        let device_id = "test-device".to_string();
        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel(HUB_OUTBOUND_CAPACITY);
        state.hubs.write().await.insert(
            device_id.clone(),
            HubHandle {
                tx: ws_tx,
                conn_id: 0,
            },
        );

        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            let _ = forward_to_hub(
                &state_clone,
                "test-device",
                "GET",
                "/test-path",
                HeaderMap::new(),
                None,
            )
            .await;
        });

        // Let the task run up to the await point inside forward_to_hub
        let msg = tokio::time::timeout(Duration::from_millis(500), ws_rx.recv())
            .await
            .expect("should receive request on ws channel")
            .expect("msg is some");

        let request_id = match msg {
            RelayMessage::Request { id, .. } => id,
            _ => panic!("expected RelayMessage::Request"),
        };

        // Assert that the request is currently registered in the pending map
        {
            assert!(
                state.pending.contains_key(&request_id),
                "request must be in pending map"
            );
        }

        // Abort the spawned task (simulating client cancel/disconnect)
        handle.abort();
        let _ = handle.await; // wait for it to be fully dropped

        // Wait a tiny bit for the drop/cleanup to complete
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Assert that the request_id is cleaned up and no longer exists in the pending map
        {
            assert!(
                !state.pending.contains_key(&request_id),
                "pending request must be cleaned up on cancel!"
            );
        }
    }

    #[tokio::test]
    async fn test_relay_disconnect_does_not_erase_device_key() {
        let state = RelayState {
            hubs: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(DashMap::new()),
            device_keys: Arc::new(RwLock::new(HashMap::new())),
            ws_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            pair_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            used_register_nonces: Arc::new(DashMap::new()),
        };

        let device_id = "device_123".to_string();
        let pub_key = [7u8; 32];
        state
            .device_keys
            .write()
            .await
            .insert(device_id.clone(), pub_key);

        // Simulate connection
        let (ws_tx, _ws_rx) = tokio::sync::mpsc::channel(HUB_OUTBOUND_CAPACITY);
        state.hubs.write().await.insert(
            device_id.clone(),
            HubHandle {
                tx: ws_tx,
                conn_id: 1,
            },
        );

        // Verify key exists
        assert!(state.device_keys.read().await.contains_key(&device_id));

        // Simulate disconnection (calling the cleanup part of handle_hub_socket)
        if let Some(id) = Some(device_id.clone()) {
            let conn_id = 1u64;
            let mut hubs = state.hubs.write().await;
            if hubs.get(&id).is_some_and(|h| h.conn_id == conn_id) {
                hubs.remove(&id);
            }
        }

        // Verify key is NOT removed after disconnect
        assert!(
            state.device_keys.read().await.contains_key(&device_id),
            "device key must be preserved after hub disconnects (SEC-M4-001)"
        );
    }

    #[tokio::test]
    async fn test_relay_disconnect_prevents_hijacking() {
        let state = RelayState {
            hubs: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(DashMap::new()),
            device_keys: Arc::new(RwLock::new(HashMap::new())),
            ws_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            pair_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            used_register_nonces: Arc::new(DashMap::new()),
        };

        let device_id = "device_123".to_string();
        let pub_key_a = [7u8; 32];
        let pub_key_b = [8u8; 32];

        // 1. First registration binds device_123 to pub_key_a
        state
            .device_keys
            .write()
            .await
            .insert(device_id.clone(), pub_key_a);

        // Simulate Hub connection
        let (ws_tx, _ws_rx) = tokio::sync::mpsc::channel(HUB_OUTBOUND_CAPACITY);
        state.hubs.write().await.insert(
            device_id.clone(),
            HubHandle {
                tx: ws_tx,
                conn_id: 1,
            },
        );

        // Verify key exists
        assert!(state.device_keys.read().await.contains_key(&device_id));

        // 2. Hub disconnects
        state.hubs.write().await.remove(&device_id);

        // 3. Hijacker attempts to register with pub_key_b
        // We simulate accept_registration's key-check logic
        let accept_hijack = {
            let keys = state.device_keys.read().await;
            if let Some(existing) = keys.get(&device_id) {
                if existing != &pub_key_b {
                    Err("device_id already bound to another key".to_string())
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        };
        assert!(accept_hijack.is_err());
        assert_eq!(
            accept_hijack.unwrap_err(),
            "device_id already bound to another key"
        );

        // 4. Legitimate Hub reconnects with pub_key_a
        let accept_legit = {
            let keys = state.device_keys.read().await;
            if let Some(existing) = keys.get(&device_id) {
                if existing != &pub_key_a {
                    Err("device_id already bound to another key".to_string())
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        };
        assert!(accept_legit.is_ok());
    }

    /// Verifies that a stale connection's cleanup does NOT evict a newer
    /// registration for the same device_id (reconnect race fix).
    #[tokio::test]
    async fn test_relay_stale_cleanup_does_not_evict_new_connection() {
        let state = RelayState {
            hubs: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(DashMap::new()),
            device_keys: Arc::new(RwLock::new(HashMap::new())),
            ws_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            pair_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            used_register_nonces: Arc::new(DashMap::new()),
        };

        let device_id = "device_reconnect".to_string();

        // Old connection registered with conn_id = 1.
        let (old_tx, _old_rx) = tokio::sync::mpsc::channel(HUB_OUTBOUND_CAPACITY);
        state.hubs.write().await.insert(
            device_id.clone(),
            HubHandle {
                tx: old_tx,
                conn_id: 1,
            },
        );

        // New connection registers and overwrites the entry with conn_id = 2.
        let (new_tx, _new_rx) = tokio::sync::mpsc::channel(HUB_OUTBOUND_CAPACITY);
        state.hubs.write().await.insert(
            device_id.clone(),
            HubHandle {
                tx: new_tx,
                conn_id: 2,
            },
        );

        // Old connection's cleanup fires with conn_id = 1; must NOT remove the
        // new entry (conn_id = 2).
        let stale_conn_id = 1u64;
        {
            let mut hubs = state.hubs.write().await;
            if hubs
                .get(&device_id)
                .is_some_and(|h| h.conn_id == stale_conn_id)
            {
                hubs.remove(&device_id);
            }
        }

        assert!(
            state.hubs.read().await.contains_key(&device_id),
            "new connection must remain registered after stale cleanup"
        );
        assert_eq!(
            state.hubs.read().await.get(&device_id).map(|h| h.conn_id),
            Some(2),
            "conn_id of remaining entry must be the new connection's"
        );
    }
}
