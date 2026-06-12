//! Hub client pairing — iLink-compatible QR endpoints + confirmation page.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::hub::pairing::PairingError;
use crate::hub::HubState;
use crate::ilink::types::{GetQrcodeResponse, QrcodeStatusResponse};

static PAIR_HTML_TEMPLATE: &str = include_str!("pair.html");

#[derive(Debug, Deserialize)]
pub struct BotQrcodeQuery {
    #[serde(default)]
    pub bot_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QrcodeStatusQuery {
    pub qrcode: String,
    /// Ignored for Hub pairing; accepted for OpenClaw / iLink SDK compatibility.
    #[serde(default)]
    pub verify_code: Option<String>,
}

/// Body sent by OpenClaw `fetchQRCode` (POST).
#[derive(Debug, Deserialize, Default)]
pub struct BotQrcodeBody {
    #[serde(default)]
    pub local_token_list: Vec<String>,
}

/// Hold long-poll requests briefly so clients (OpenClaw) can wait on one HTTP call.
const QR_STATUS_LONG_POLL: Duration = Duration::from_secs(25);
const QR_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
pub struct PairConfirmRequest {
    pub name: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PairConfirmResponse {
    pub ret: i32,
    pub name: String,
    pub vtoken: String,
}

/// Device id for zero-config relay pairing (lazy-loaded once per process).
fn pairing_device_id() -> String {
    use std::sync::OnceLock;
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    DEVICE_ID
        .get_or_init(|| {
            crate::relay::DeviceIdentity::load_or_create()
                .map(|id| id.device_id().to_string())
                .unwrap_or_else(|e| {
                    warn!(error = %e, "failed to load device identity, using ephemeral id");
                    uuid::Uuid::new_v4().to_string()
                })
        })
        .clone()
}

/// Public URL embedded in pairing QR codes (must be reachable from a phone).
fn pair_public_url() -> String {
    crate::relay::resolve_pair_public_url(&pairing_device_id())
}

/// API base URL returned to iLink clients after pairing (usually localhost).
fn client_base_url() -> String {
    std::env::var("HUB_CLIENT_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_string())
}

pub async fn register_client_in_hub(
    state: &HubState,
    name: String,
    label: Option<String>,
) -> String {
    // Lock order: registry → router (always).
    let (vtoken, is_first) = {
        let mut registry = state.registry.write().await;
        let vtoken = registry.register(name.clone(), label.clone());
        let is_first = registry.all_clients().len() == 1;
        (vtoken, is_first)
    };

    if is_first {
        let mut router = state.router.lock().await;
        router.set_default(vtoken.clone());
    }

    if let Err(e) = state
        .store
        .upsert_client(&vtoken, &name, label.as_deref())
        .await
    {
        warn!(error = %e, name = %name, "failed to persist paired client");
    }

    vtoken
}

#[derive(Debug)]
pub enum UnregisterClientError {
    NotFound,
    StillOnline,
    Store(anyhow::Error),
}

/// Remove a registered backend client from memory, DB, routing, and its message queue.
/// Only offline clients can be deleted.
pub async fn unregister_client_in_hub(
    state: &HubState,
    name: &str,
) -> Result<(), UnregisterClientError> {
    let vtoken = {
        let registry = state.registry.read().await;
        let Some(client) = registry.get_by_name(name) else {
            return Err(UnregisterClientError::NotFound);
        };
        if client.online {
            return Err(UnregisterClientError::StillOnline);
        }
        client.vtoken.clone()
    };

    // Lock order: registry → router (always). Drop registry before acquiring router.
    let new_default = {
        let mut registry = state.registry.write().await;
        if !registry.remove(name) {
            return Err(UnregisterClientError::NotFound);
        }
        registry.pick_default_after_remove(&vtoken)
    };
    {
        let mut router = state.router.lock().await;
        router.remove_routes_for_vtoken(&vtoken, new_default);
    }

    if let Err(e) = state.queue.remove_client(&vtoken).await {
        warn!(error = %e, vtoken = %&vtoken[..vtoken.len().min(8)], "failed to remove client queue");
    }

    state
        .store
        .clear_routes_for_vtoken(&vtoken)
        .await
        .map_err(UnregisterClientError::Store)?;
    state
        .store
        .delete_client_by_name(name)
        .await
        .map_err(UnregisterClientError::Store)?;

    info!(client = %name, vtoken = %&vtoken[..vtoken.len().min(8)], "admin deleted offline client");
    Ok(())
}

#[derive(Debug)]
pub enum UpdateClientError {
    NotFound,
    NameTaken,
    InvalidName,
    Store(anyhow::Error),
}

/// Update a registered client's name and label in memory and DB.
pub async fn update_client_in_hub(
    state: &HubState,
    old_name: &str,
    new_name: &str,
    label: Option<String>,
) -> Result<String, UpdateClientError> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(UpdateClientError::InvalidName);
    }

    let label_for_store = label.clone();
    let vtoken = {
        let mut registry = state.registry.write().await;
        registry
            .update_client(old_name, new_name, label)
            .map_err(|e| match e {
                crate::hub::registry::UpdateClientError::NotFound => UpdateClientError::NotFound,
                crate::hub::registry::UpdateClientError::NameTaken => UpdateClientError::NameTaken,
            })?
    };

    state
        .store
        .update_client_by_vtoken(&vtoken, new_name, label_for_store.as_deref())
        .await
        .map_err(UpdateClientError::Store)?;

    info!(
        old_name = %old_name,
        new_name = %new_name,
        vtoken = %&vtoken[..vtoken.len().min(8)],
        "admin updated client"
    );
    Ok(vtoken)
}

fn build_pairing_qr_response(code: String) -> GetQrcodeResponse {
    let base = pair_public_url();
    let pair_url = crate::relay::pair_qr_url(&base, &code);
    info!(code = %code, pair_url = %pair_url, "pairing QR session created");
    GetQrcodeResponse {
        ret: 0,
        qrcode: Some(code),
        qrcode_img_content: Some(pair_url),
        errmsg: None,
    }
}

async fn create_pairing_qr(state: &HubState) -> GetQrcodeResponse {
    let code = {
        let mut pairing = state.pairing.write().await;
        pairing.create()
    };
    build_pairing_qr_response(code)
}

/// `GET /ilink/bot/get_bot_qrcode` — start a Hub pairing session (not WeChat login).
pub async fn get_bot_qrcode(
    State(state): State<Arc<HubState>>,
    Query(_query): Query<BotQrcodeQuery>,
) -> Json<GetQrcodeResponse> {
    Json(create_pairing_qr(state.as_ref()).await)
}

/// `POST /ilink/bot/get_bot_qrcode` — OpenClaw sends `local_token_list` in the body.
pub async fn get_bot_qrcode_post(
    State(state): State<Arc<HubState>>,
    Query(_query): Query<BotQrcodeQuery>,
    Json(body): Json<BotQrcodeBody>,
) -> Json<GetQrcodeResponse> {
    if !body.local_token_list.is_empty() {
        info!(
            count = body.local_token_list.len(),
            "get_bot_qrcode POST (local_token_list ignored for hub pairing)"
        );
    }
    Json(create_pairing_qr(state.as_ref()).await)
}

async fn qrcode_status_json(state: &HubState, qrcode: &str) -> QrcodeStatusResponse {
    let session = {
        let pairing = state.pairing.read().await;
        pairing.get(qrcode)
    };

    let Some(session) = session else {
        return QrcodeStatusResponse {
            ret: -1,
            status: Some("expired".to_string()),
            bot_token: None,
            baseurl: None,
            ilink_bot_id: None,
            ilink_user_id: None,
            errmsg: Some("pairing session not found".to_string()),
        };
    };

    let client_base = client_base_url();
    let status = session.status_str().to_string();
    let bot_token = session.vtoken.clone();

    QrcodeStatusResponse {
        ret: 0,
        status: Some(status),
        bot_token,
        baseurl: if session.status_str() == "confirmed" {
            Some(client_base)
        } else {
            None
        },
        ilink_bot_id: Some("ilink-hub@hub.local".to_string()),
        ilink_user_id: Some("hub-client".to_string()),
        errmsg: None,
    }
}

/// `GET /ilink/bot/get_qrcode_status` — poll pairing progress (long-poll friendly).
pub async fn get_qrcode_status(
    State(state): State<Arc<HubState>>,
    Query(query): Query<QrcodeStatusQuery>,
) -> Json<QrcodeStatusResponse> {
    if query.verify_code.is_some() {
        info!("verify_code ignored for hub client pairing");
    }

    let deadline = Instant::now() + QR_STATUS_LONG_POLL;
    loop {
        let resp = qrcode_status_json(state.as_ref(), &query.qrcode).await;
        let terminal = resp.status.as_deref() != Some("wait");
        if terminal || Instant::now() >= deadline {
            return Json(resp);
        }
        tokio::time::sleep(QR_STATUS_POLL_INTERVAL).await;
    }
}

/// `GET /hub/pair/{code}` — mobile-friendly confirmation page.
pub async fn pair_page(
    State(state): State<Arc<HubState>>,
    Path(code): Path<String>,
) -> impl IntoResponse {
    let session = {
        let mut pairing = state.pairing.write().await;
        if pairing.get(&code).is_some() {
            pairing.mark_scanned(&code);
        }
        pairing.get(&code)
    };

    let Some(session) = session else {
        return (
            StatusCode::NOT_FOUND,
            Html("<h1>配对码无效或已过期</h1><p>请回到客户端重新获取二维码。</p>".to_string()),
        )
            .into_response();
    };

    if session.status_str() == "expired" {
        return (
            StatusCode::GONE,
            Html("<h1>配对码已过期</h1><p>请回到客户端重新获取二维码。</p>".to_string()),
        )
            .into_response();
    }

    if session.status_str() == "confirmed" {
        let name = session.client_name.as_deref().unwrap_or("client");
        return (
            StatusCode::OK,
            Html(format!(
                "<h1>已配对</h1><p>客户端 <strong>{name}</strong> 已成功接入。</p>"
            )),
        )
            .into_response();
    }

    let html = PAIR_HTML_TEMPLATE.replace("__PAIR_CODE__", &code);
    (StatusCode::OK, Html(html)).into_response()
}

/// `POST /hub/pair/{code}/confirm` — approve pairing and issue vtoken.
pub async fn pair_confirm(
    State(state): State<Arc<HubState>>,
    Path(code): Path<String>,
    Json(req): Json<PairConfirmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name is required" })),
        );
    }

    let label = req
        .label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());

    let check_result = {
        let pairing = state.pairing.read().await;
        if let Some(session) = pairing.get(&code) {
            match session.public_status() {
                crate::hub::pairing::PairingStatus::Expired => Err(PairingError::Expired),
                crate::hub::pairing::PairingStatus::Confirmed => Err(PairingError::AlreadyConfirmed),
                _ => Ok(()),
            }
        } else {
            Err(PairingError::NotFound)
        }
    };

    if let Err(e) = check_result {
        return match e {
            PairingError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "pairing session not found" })),
            ),
            PairingError::Expired => (
                StatusCode::GONE,
                Json(serde_json::json!({ "error": "pairing session expired" })),
            ),
            PairingError::AlreadyConfirmed => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "pairing already confirmed" })),
            ),
        };
    }

    let vtoken = register_client_in_hub(state.as_ref(), name.clone(), label.clone()).await;

    let confirm_result = {
        let mut pairing = state.pairing.write().await;
        pairing.confirm(&code, name.clone(), label, vtoken.clone())
    };

    match confirm_result {
        Ok(()) => {
            info!(code = %code, name = %name, "pairing confirmed");
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ret": 0,
                    "name": name,
                    "vtoken": vtoken,
                })),
            )
        }
        Err(PairingError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "pairing session not found" })),
        ),
        Err(PairingError::Expired) => (
            StatusCode::GONE,
            Json(serde_json::json!({ "error": "pairing session expired" })),
        ),
        Err(PairingError::AlreadyConfirmed) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "pairing already confirmed" })),
        ),
    }
}
