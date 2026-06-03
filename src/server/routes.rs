/// iLink-compatible HTTP routes exposed to backend clients.
/// Clients configure `base_url = https://your-hub.example.com` and
/// use their virtual token — they see the exact same API as ilinkai.weixin.qq.com.

use std::sync::Arc;
use std::time::Duration;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::hub::HubState;
use crate::ilink::types::*;

// ─── Auth helper ─────────────────────────────────────────────────────────────

fn extract_vtoken(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
}

// ─── Registration (Hub-specific, non-iLink) ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub ret: i32,
    pub vtoken: String,
    pub base_url: String,
    pub errmsg: Option<String>,
}

pub async fn register(
    State(state): State<Arc<HubState>>,
    Json(req): Json<RegisterRequest>,
) -> Json<RegisterResponse> {
    let vtoken = {
        let mut registry = state.registry.write().await;
        registry.register(req.name.clone(), req.label)
    };

    // Ensure queue exists
    {
        let mut queues = state.queues.lock().await;
        queues.ensure(&vtoken);
    }

    // Set as default if it's the first client
    {
        let mut router = state.router.lock().await;
        let registry = state.registry.read().await;
        if registry.online_clients().len() == 1 {
            router.set_default(vtoken.clone());
        }
    }

    Json(RegisterResponse {
        ret: 0,
        vtoken: vtoken.clone(),
        base_url: String::new(), // filled by the server layer
        errmsg: None,
    })
}

// ─── getupdates (long-poll) ───────────────────────────────────────────────────

pub async fn getupdates(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(req): Json<GetUpdatesRequest>,
) -> (StatusCode, Json<GetUpdatesResponse>) {
    let Some(vtoken) = extract_vtoken(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(GetUpdatesResponse {
                ret: 401,
                errmsg: Some("Missing Authorization header".to_string()),
                buf: None,
                list: None,
            }),
        );
    };

    // Mark client as online
    {
        let mut registry = state.registry.write().await;
        registry.mark_seen(&vtoken);
    }

    let poll_secs = req.timeout.unwrap_or(30).min(60);
    let notify = {
        let queues = state.queues.lock().await;
        queues.notify_handle(&vtoken)
    };

    if let Some(notify) = notify {
        // Wait until there are messages or timeout
        let _ = timeout(Duration::from_secs(poll_secs as u64), notify.notified()).await;
    } else {
        // Client not registered — just wait
        tokio::time::sleep(Duration::from_secs(poll_secs as u64)).await;
    }

    let messages = {
        let mut queues = state.queues.lock().await;
        queues.drain(&vtoken)
    };

    debug!(vtoken = %vtoken, count = messages.len(), "getupdates returning");

    (
        StatusCode::OK,
        Json(GetUpdatesResponse {
            ret: 0,
            errmsg: None,
            buf: Some(String::new()), // clients don't need real cursor since Hub manages it
            list: if messages.is_empty() { None } else { Some(messages) },
        }),
    )
}

// ─── sendmessage ─────────────────────────────────────────────────────────────

pub async fn sendmessage(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(mut req): Json<SendMessageRequest>,
) -> Json<SendMessageResponse> {
    let Some(_vtoken) = extract_vtoken(&headers) else {
        return Json(SendMessageResponse {
            ret: 401,
            errmsg: Some("Missing Authorization header".to_string()),
        });
    };

    // Translate virtual context token → real context token
    let real_ctx = {
        let ctx_map = state.ctx_map.lock().await;
        ctx_map.resolve(&req.context_token).map(str::to_string)
    };

    let Some(real_ctx) = real_ctx else {
        warn!(vctx = %req.context_token, "no mapping for virtual context token");
        return Json(SendMessageResponse {
            ret: 400,
            errmsg: Some("Unknown context_token".to_string()),
        });
    };

    req.context_token = real_ctx;

    match state.upstream.send_message(req).await {
        Ok(resp) => Json(resp),
        Err(e) => Json(SendMessageResponse {
            ret: 500,
            errmsg: Some(format!("upstream error: {e}")),
        }),
    }
}

// ─── sendtyping ──────────────────────────────────────────────────────────────

pub async fn sendtyping(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(mut req): Json<SendTypingRequest>,
) -> Json<serde_json::Value> {
    let Some(_vtoken) = extract_vtoken(&headers) else {
        return Json(serde_json::json!({"ret": 401, "errmsg": "Missing Authorization"}));
    };

    // Translate context token
    let real_ctx = {
        let ctx_map = state.ctx_map.lock().await;
        ctx_map.resolve(&req.context_token).map(str::to_string)
    };

    if let Some(real_ctx) = real_ctx {
        req.context_token = real_ctx;
        let _ = state.upstream.send_typing(req).await;
    }

    Json(serde_json::json!({"ret": 0}))
}

// ─── getconfig ───────────────────────────────────────────────────────────────

pub async fn getconfig(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(mut req): Json<GetConfigRequest>,
) -> Json<GetConfigResponse> {
    let Some(_vtoken) = extract_vtoken(&headers) else {
        return Json(GetConfigResponse {
            ret: 401,
            typing_ticket: None,
            errmsg: Some("Missing Authorization".to_string()),
        });
    };

    // Translate context token
    let real_ctx = {
        let ctx_map = state.ctx_map.lock().await;
        ctx_map.resolve(&req.context_token).map(str::to_string)
    };

    if let Some(real_ctx) = real_ctx {
        req.context_token = real_ctx;
    }

    match state.upstream.get_config(req).await {
        Ok(resp) => Json(resp),
        Err(e) => Json(GetConfigResponse {
            ret: 500,
            typing_ticket: None,
            errmsg: Some(format!("upstream error: {e}")),
        }),
    }
}

// ─── getuploadurl ─────────────────────────────────────────────────────────────

pub async fn getuploadurl(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(req): Json<GetUploadUrlRequest>,
) -> Json<GetUploadUrlResponse> {
    let Some(_vtoken) = extract_vtoken(&headers) else {
        return Json(GetUploadUrlResponse {
            ret: 401,
            upload_url: None,
            media_id: None,
            errmsg: Some("Missing Authorization".to_string()),
        });
    };

    match state.upstream.get_upload_url(req).await {
        Ok(resp) => Json(resp),
        Err(e) => Json(GetUploadUrlResponse {
            ret: 500,
            upload_url: None,
            media_id: None,
            errmsg: Some(format!("upstream error: {e}")),
        }),
    }
}

// ─── Admin: list clients ──────────────────────────────────────────────────────

pub async fn admin_clients(State(state): State<Arc<HubState>>) -> Json<serde_json::Value> {
    let registry = state.registry.read().await;
    let clients: Vec<_> = registry
        .all_clients()
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "label": c.label,
                "online": c.online,
            })
        })
        .collect();
    Json(serde_json::json!({ "clients": clients }))
}
