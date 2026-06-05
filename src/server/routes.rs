//! iLink-compatible HTTP routes exposed to backend clients.
//! Clients configure `base_url = https://your-hub.example.com` and
//! use their virtual token — they see the exact same API as ilinkai.weixin.qq.com.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, error, warn};

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

/// Check admin token if `ILINK_ADMIN_TOKEN` env is set.
/// Returns `true` (allowed) when:
///   - The env var is not set (open access, for local dev)
///   - The env var is set and the `Authorization: Bearer <token>` matches
fn check_admin_auth(headers: &HeaderMap) -> bool {
    let Ok(required) = std::env::var("ILINK_ADMIN_TOKEN") else {
        return true; // not configured → unrestricted
    };
    if required.is_empty() {
        return true;
    }
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    provided == required
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
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> (StatusCode, Json<RegisterResponse>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(RegisterResponse {
                ret: 401,
                vtoken: String::new(),
                base_url: String::new(),
                errmsg: Some("Unauthorized".to_string()),
            }),
        );
    }

    let vtoken = {
        let mut registry = state.registry.write().await;
        registry.register(req.name.clone(), req.label.clone())
    };

    // Set as default if it's the first online client
    {
        let mut router = state.router.lock().await;
        let registry = state.registry.read().await;
        if registry.online_clients().len() == 1 {
            router.set_default(vtoken.clone());
        }
    }

    // Persist to DB (best-effort)
    if let Err(e) = state
        .store
        .upsert_client(&vtoken, &req.name, req.label.as_deref())
        .await
    {
        warn!(error = %e, name = %req.name, "failed to persist client registration to DB");
    }

    (
        StatusCode::OK,
        Json(RegisterResponse {
            ret: 0,
            vtoken: vtoken.clone(),
            base_url: String::new(), // filled by the server layer
            errmsg: None,
        }),
    )
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
    let _ = state
        .queue
        .wait_notify(&vtoken, poll_secs as u64)
        .await
        .unwrap_or_else(|e| {
            error!(error = %e, vtoken = %vtoken, "wait_notify failed");
            false
        });

    let messages = state.queue.drain(&vtoken).await.unwrap_or_else(|e| {
        error!(error = %e, vtoken = %vtoken, "queue drain failed");
        vec![]
    });

    debug!(vtoken = %vtoken, count = messages.len(), "getupdates returning");

    (
        StatusCode::OK,
        Json(GetUpdatesResponse {
            ret: 0,
            errmsg: None,
            buf: Some(String::new()), // clients don't need real cursor since Hub manages it
            list: if messages.is_empty() {
                None
            } else {
                Some(messages)
            },
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

    // Translate virtual context token → real context token (memory first, DB fallback)
    let real_ctx = {
        let ctx_map = state.ctx_map.lock().await;
        ctx_map.resolve(&req.context_token).map(str::to_string)
    };

    let real_ctx = match real_ctx {
        Some(ctx) => ctx,
        None => {
            // Memory miss → try DB (covers restarts where memory cache was cold)
            match state.store.resolve_context_token(&req.context_token).await {
                Ok(Some(ctx)) => {
                    // Warm the in-memory cache
                    let mut ctx_map = state.ctx_map.lock().await;
                    ctx_map.seed(req.context_token.clone(), ctx.clone());
                    ctx
                }
                Ok(None) => {
                    warn!(vctx = %req.context_token, "no mapping for virtual context token");
                    return Json(SendMessageResponse {
                        ret: 400,
                        errmsg: Some("Unknown context_token".to_string()),
                    });
                }
                Err(e) => {
                    warn!(error = %e, vctx = %req.context_token, "DB lookup for context_token failed");
                    return Json(SendMessageResponse {
                        ret: 500,
                        errmsg: Some("context_token resolution error".to_string()),
                    });
                }
            }
        }
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

pub async fn admin_clients(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        );
    }

    let registry = state.registry.read().await;
    let clients: Vec<_> = registry
        .all_clients()
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "label": c.label,
                "online": c.online,
                "vtoken": c.vtoken,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "clients": clients })),
    )
}

// ─── Web Admin UI ─────────────────────────────────────────────────────────────

static ADMIN_HTML: &str = include_str!("admin.html");

pub async fn admin_ui() -> axum::response::Html<&'static str> {
    axum::response::Html(ADMIN_HTML)
}

// ─── Metrics (Prometheus text format) ────────────────────────────────────────

pub async fn metrics(State(state): State<Arc<HubState>>) -> (StatusCode, String) {
    let (online, total, client_names_by_vtoken) = {
        let registry = state.registry.read().await;
        let online = registry.online_clients().len() as u64;
        let total = registry.all_clients().len() as u64;
        let names: std::collections::HashMap<String, String> = registry
            .all_clients()
            .iter()
            .map(|c| (c.vtoken.clone(), c.name.clone()))
            .collect();
        (online, total, names)
    };

    let queue_sizes = state.queue.queue_sizes().await.unwrap_or_else(|e| {
        error!(error = %e, "queue_sizes failed");
        std::collections::HashMap::new()
    });

    let messages_dispatched = state.metrics.messages_dispatched.load(Ordering::Relaxed);
    let messages_dropped = state.metrics.messages_dropped.load(Ordering::Relaxed);
    let upstream_polls_ok = state.upstream.polls_ok.load(Ordering::Relaxed);
    let upstream_polls_err = state.upstream.polls_err.load(Ordering::Relaxed);

    let mut out = String::with_capacity(1024);

    out.push_str("# HELP ilink_hub_clients_online Number of online clients\n");
    out.push_str("# TYPE ilink_hub_clients_online gauge\n");
    out.push_str(&format!("ilink_hub_clients_online {}\n", online));

    out.push_str("# HELP ilink_hub_clients_total Total registered clients\n");
    out.push_str("# TYPE ilink_hub_clients_total gauge\n");
    out.push_str(&format!("ilink_hub_clients_total {}\n", total));

    out.push_str(
        "# HELP ilink_hub_messages_dispatched_total Messages dispatched to client queues\n",
    );
    out.push_str("# TYPE ilink_hub_messages_dispatched_total counter\n");
    out.push_str(&format!(
        "ilink_hub_messages_dispatched_total {}\n",
        messages_dispatched
    ));

    out.push_str("# HELP ilink_hub_messages_dropped_total Messages dropped (no online clients or queue overflow)\n");
    out.push_str("# TYPE ilink_hub_messages_dropped_total counter\n");
    out.push_str(&format!(
        "ilink_hub_messages_dropped_total {}\n",
        messages_dropped
    ));

    out.push_str(
        "# HELP ilink_hub_upstream_polls_ok_total Successful upstream long-poll responses\n",
    );
    out.push_str("# TYPE ilink_hub_upstream_polls_ok_total counter\n");
    out.push_str(&format!(
        "ilink_hub_upstream_polls_ok_total {}\n",
        upstream_polls_ok
    ));

    out.push_str("# HELP ilink_hub_upstream_polls_err_total Failed upstream long-poll responses\n");
    out.push_str("# TYPE ilink_hub_upstream_polls_err_total counter\n");
    out.push_str(&format!(
        "ilink_hub_upstream_polls_err_total {}\n",
        upstream_polls_err
    ));

    out.push_str("# HELP ilink_hub_queue_size Current pending message count per client\n");
    out.push_str("# TYPE ilink_hub_queue_size gauge\n");
    for (vtoken, size) in &queue_sizes {
        let name = client_names_by_vtoken
            .get(vtoken)
            .map(String::as_str)
            .unwrap_or("unknown");
        out.push_str(&format!(
            "ilink_hub_queue_size{{client=\"{}\"}} {}\n",
            name, size
        ));
    }

    (StatusCode::OK, out)
}
