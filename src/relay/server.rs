//! Public pairing relay server — forwards phone HTTP to connected Hubs via WebSocket.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

// Per-IP: WS handshakes (Hub reconnects every ~5s → need headroom); pairing HTTP from phones.
const WS_RATE_MAX: usize = 120;
const WS_RATE_WINDOW_SECS: u64 = 600;
const PAIR_RATE_MAX: usize = 30;
const PAIR_RATE_WINDOW_SECS: u64 = 60;

#[derive(Clone)]
pub struct RelayState {
    hubs: Arc<RwLock<HashMap<String, HubHandle>>>,
    pending: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<RelayMessage>>>>,
    device_keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
    ws_rate_limiter: Arc<RateLimiter>,
    pair_rate_limiter: Arc<RateLimiter>,
}

#[derive(Clone)]
struct HubHandle {
    tx: tokio::sync::mpsc::UnboundedSender<RelayMessage>,
}

pub fn build_relay_router() -> Router {
    let state = RelayState {
        hubs: Arc::new(RwLock::new(HashMap::new())),
        pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        device_keys: Arc::new(RwLock::new(HashMap::new())),
        ws_rate_limiter: Arc::new(RateLimiter::new(WS_RATE_MAX, WS_RATE_WINDOW_SECS)),
        pair_rate_limiter: Arc::new(RateLimiter::new(PAIR_RATE_MAX, PAIR_RATE_WINDOW_SECS)),
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
    let (mut write, mut read) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<RelayMessage>();
    let mut device_id: Option<String> = None;

    let register_deadline = tokio::time::sleep(REGISTER_TIMEOUT);
    tokio::pin!(register_deadline);

    loop {
        tokio::select! {
            _ = &mut register_deadline, if device_id.is_none() => {
                warn!("hub registration timeout");
                break;
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
                        match accept_registration(&state, &id, &public_key, timestamp, &signature).await {
                            Ok(()) => {
                                state.hubs.write().await.insert(
                                    id.clone(),
                                    HubHandle { tx: out_tx.clone() },
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
                        let tx = state.pending.lock().unwrap().remove(&id);
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
        state.hubs.write().await.remove(&id);
        info!(device_id = %id, "hub disconnected");
    }
}

struct PendingRequestGuard {
    pending: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<RelayMessage>>>>,
    request_id: String,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.request_id);
        }
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

    state.pending.lock().unwrap().insert(request_id.clone(), tx);

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

    if hub.tx.send(req).is_err() {
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
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(status) = check_pair_rate(&state, &addr) {
        return (status, "too many pairing requests").into_response();
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
    .await?;
    Ok(())
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

    #[tokio::test]
    async fn test_relay_pending_request_no_leak_on_cancel() {
        let state = RelayState {
            hubs: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            device_keys: Arc::new(RwLock::new(HashMap::new())),
            ws_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
            pair_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
        };

        let device_id = "test-device".to_string();
        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel();
        state
            .hubs
            .write()
            .await
            .insert(device_id.clone(), HubHandle { tx: ws_tx });

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
            let pending = state.pending.lock().unwrap();
            assert!(
                pending.contains_key(&request_id),
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
            let pending = state.pending.lock().unwrap();
            assert!(
                !pending.contains_key(&request_id),
                "pending request must be cleaned up on cancel!"
            );
        }
    }
}
