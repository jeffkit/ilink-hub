//! iLink-compatible HTTP routes exposed to backend clients.
//! Clients configure `base_url = https://your-hub.example.com` and
//! use their virtual token — they see the same API as ilinkai.weixin.qq.com.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, error, warn};

use crate::hub::{HubState, MessageQueue};
use crate::ilink::types::*;
use crate::server::pairing::register_client_in_hub;

/// Returned when a downstream `Authorization` vtoken is not in the Hub registry.
pub const UNKNOWN_VTOKEN_MSG: &str = "Unknown or revoked virtual token; register via POST /hub/register or ilink-hub-bridge --force-register";

// ─── Auth helper ─────────────────────────────────────────────────────────────

/// Redact a virtual token for logging: show only the first 8 characters followed by `…`.
/// This lets operators correlate log lines without exposing the full credential.
fn redact_token(t: &str) -> String {
    let prefix: String = t.chars().take(8).collect();
    format!("{prefix}…")
}

fn extract_vtoken(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Returns the configured admin token, reading env once per process.
fn admin_token() -> Option<&'static str> {
    use std::sync::OnceLock;
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            std::env::var("ILINK_ADMIN_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .as_deref()
}

/// Returns `true` when the request should be allowed through to an admin endpoint.
///
/// Auth logic:
/// - `ILINK_ADMIN_TOKEN` set → Bearer token must match exactly.
/// - `ILINK_ADMIN_TOKEN` not set AND `ILINK_ADMIN_INSECURE_NO_AUTH=true` → allow (with a
///   loud startup warning logged separately).
/// - `ILINK_ADMIN_TOKEN` not set and insecure flag absent → deny with 403.
fn check_admin_auth(headers: &HeaderMap) -> bool {
    if let Some(required) = admin_token() {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        return provided == required;
    }
    // No token configured — only allow if the operator explicitly opts in to insecure mode.
    insecure_no_auth()
}

fn insecure_no_auth() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        let enabled = std::env::var("ILINK_ADMIN_INSECURE_NO_AUTH")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if enabled {
            tracing::warn!(
                "ILINK_ADMIN_INSECURE_NO_AUTH is set — admin endpoints have NO authentication. \
                 Never expose this server to the public internet."
            );
        }
        enabled
    })
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

    let vtoken = register_client_in_hub(state.as_ref(), req.name.clone(), req.label.clone()).await;

    (
        StatusCode::OK,
        Json(RegisterResponse {
            ret: 0,
            vtoken: vtoken.clone(),
            base_url: String::new(),
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
                ret: Some(401),
                errcode: None,
                errmsg: Some("Missing Authorization header".to_string()),
                msgs: None,
                get_updates_buf: None,
            }),
        );
    };

    {
        let registry = state.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "getupdates rejected: unknown virtual token");
            return (
                StatusCode::UNAUTHORIZED,
                Json(GetUpdatesResponse {
                    ret: Some(401),
                    errcode: None,
                    errmsg: Some(UNKNOWN_VTOKEN_MSG.to_string()),
                    msgs: None,
                    get_updates_buf: None,
                }),
            );
        }
    }

    {
        let mut registry = state.registry.write().await;
        registry.mark_seen(&vtoken);
    }

    // Use timeout from legacy field if provided, otherwise 30s
    let poll_secs = req.timeout.unwrap_or(30).min(60) as u64;
    let mut shutdown_rx = state.shutdown.clone();
    let notified =
        wait_notify_or_shutdown(state.queue.as_ref(), &mut shutdown_rx, &vtoken, poll_secs).await;
    if !notified && *state.shutdown.borrow() {
        debug!(vtoken = %redact_token(&vtoken), "getupdates returning early due to shutdown");
    }

    let messages = state.queue.drain(&vtoken).await.unwrap_or_else(|e| {
        error!(error = %e, vtoken = %redact_token(&vtoken), "queue drain failed");
        vec![]
    });

    debug!(vtoken = %redact_token(&vtoken), count = messages.len(), "getupdates returning");

    (
        StatusCode::OK,
        Json(GetUpdatesResponse {
            ret: Some(0),
            errcode: None,
            errmsg: None,
            get_updates_buf: Some(String::new()),
            msgs: if messages.is_empty() {
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
    let Some(vtoken) = extract_vtoken(&headers) else {
        return Json(SendMessageResponse::err(
            401,
            "Missing Authorization header",
        ));
    };

    {
        let registry = state.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "sendmessage rejected: unknown virtual token");
            return Json(SendMessageResponse::err(401, UNKNOWN_VTOKEN_MSG));
        }
    }

    // Extract context_token from req.msg
    let vctx = match req.msg.as_ref().and_then(|m| m.context_token.as_deref()) {
        Some(ctx) if !ctx.is_empty() => ctx.to_string(),
        _ => {
            return Json(SendMessageResponse::err(400, "Missing msg.context_token"));
        }
    };

    // Translate virtual → real context token + get peer_user_id (memory first, DB fallback)
    let (real_ctx, peer_user_id) = {
        let ctx_map = state.ctx_map.read().await;
        ctx_map
            .resolve_full(&vctx)
            .map(|(r, p)| (r.to_string(), p.to_string()))
    }
    .unwrap_or_else(|| ("".to_string(), "".to_string()));

    let (real_ctx, peer_user_id) = if real_ctx.is_empty() {
        match state.store.resolve_context_token_full(&vctx).await {
            Ok(Some((r, p))) => {
                let mut ctx_map = state.ctx_map.write().await;
                ctx_map.seed_full(vctx.clone(), r.clone(), p.clone());
                (r, p)
            }
            Ok(None) => {
                warn!(vctx = %vctx, "no mapping for virtual context token");
                return Json(SendMessageResponse::err(400, "Unknown context_token"));
            }
            Err(e) => {
                warn!(error = %e, vctx = %vctx, "DB lookup for context_token failed");
                return Json(SendMessageResponse::err(
                    500,
                    "context_token resolution error",
                ));
            }
        }
    } else {
        (real_ctx, peer_user_id)
    };

    if let Some(msg) = &mut req.msg {
        // Extract the session name echoed back by the bridge (set since the race-condition fix).
        // This tells us which session was active when the *original message was dispatched*,
        // which may differ from the current active session if the user ran `/session new` or
        // `/session use` while the AI was processing the reply.
        let replied_session_name: Option<String> = msg
            .ilink_hub_ext
            .as_ref()
            .and_then(|e| e.session_name.as_ref())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string());

        // Read cli_session_id from hub_ext and persist it to the correct session.
        if let Some(ext) = msg.ilink_hub_ext.as_mut() {
            if let Some(cli_sid) = ext.cli_session_id.take() {
                let t = cli_sid.trim().to_string();
                if !t.is_empty() {
                    // Prefer the session name echoed by the bridge; only fall back to the
                    // current active session when the bridge didn't provide one (older clients).
                    let session_name = match replied_session_name.clone() {
                        Some(n) => n,
                        None => state
                            .store
                            .get_active_session_name(&vctx, &vtoken)
                            .await
                            .unwrap_or_else(|_| "default".to_string()),
                    };
                    if let Err(e) = state
                        .store
                        .set_backend_session(&vctx, &vtoken, &session_name, &t)
                        .await
                    {
                        warn!(error = %e, vctx = %vctx, "failed to persist backend session");
                    }
                }
            }
        }
        // Strip ilink_hub_ext before forwarding to upstream iLink.
        msg.ilink_hub_ext = None;

        // Replace virtual context_token with real one and inject to_user_id if missing
        msg.context_token = Some(real_ctx);
        if msg.to_user_id.is_none() && !peer_user_id.is_empty() {
            msg.to_user_id = Some(peer_user_id);
        }
        msg.ensure_outbound();

        let (client_meta, registered_count) = {
            let reg = state.registry.read().await;
            (
                reg.get_by_vtoken(&vtoken)
                    .map(|i| (i.name.clone(), i.label.clone())),
                reg.online_clients().len(),
            )
        };

        // Use the session name from the bridge reply when available; fall back to current
        // active session only for older bridge clients that don't echo it back.
        let active_session = match replied_session_name.clone() {
            Some(n) => Some(n),
            None => state.store.get_active_session_name(&vctx, &vtoken).await.ok(),
        };

        let env_label = std::env::var("ILINKHUB_OUTBOUND_ORIGIN_LABEL").ok();
        if crate::hub::should_append_outbound_origin_label(registered_count, env_label.as_deref()) {
            if let Some((ref name, ref label)) = client_meta {
                crate::hub::append_outbound_origin_footer_to_first_text_item(
                    msg,
                    name,
                    label.as_deref(),
                    active_session.as_deref(),
                );
            }
        }

        if let Some(cid) = msg.client_id.as_deref().filter(|s| !s.is_empty()) {
            if let Some((name, label)) = client_meta {
                let mut q = state.quote_index.lock().await;
                q.register_pending_client(cid, vtoken.clone(), name, label, active_session);
            }
        }
    }
    if req.base_info.is_none() {
        req.base_info = Some(BaseInfo::default());
    }

    match state.upstream.send_message(req).await {
        Ok(resp) => Json(resp),
        Err(e) => Json(SendMessageResponse::err(
            500,
            format!("upstream error: {e}"),
        )),
    }
}

// ─── sendtyping ──────────────────────────────────────────────────────────────

pub async fn sendtyping(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(req): Json<SendTypingRequest>,
) -> Json<serde_json::Value> {
    let Some(vtoken) = extract_vtoken(&headers) else {
        return Json(serde_json::json!({"ret": 401, "errmsg": "Missing Authorization"}));
    };
    {
        let registry = state.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "sendtyping rejected: unknown virtual token");
            return Json(serde_json::json!({"ret": 401, "errmsg": UNKNOWN_VTOKEN_MSG}));
        }
    }

    let _ = state.upstream.send_typing(req).await;
    Json(serde_json::json!({"ret": 0}))
}

// ─── getconfig ───────────────────────────────────────────────────────────────

pub async fn getconfig(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(mut req): Json<GetConfigRequest>,
) -> Json<GetConfigResponse> {
    let Some(vtoken) = extract_vtoken(&headers) else {
        return Json(GetConfigResponse {
            ret: Some(401),
            typing_ticket: None,
            errmsg: Some("Missing Authorization".to_string()),
        });
    };
    {
        let registry = state.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "getconfig rejected: unknown virtual token");
            return Json(GetConfigResponse {
                ret: Some(401),
                typing_ticket: None,
                errmsg: Some(UNKNOWN_VTOKEN_MSG.to_string()),
            });
        }
    }

    // Translate virtual context token if present
    if let Some(vctx) = &req.context_token.clone() {
        let real_ctx = {
            let ctx_map = state.ctx_map.read().await;
            ctx_map.resolve(vctx).map(str::to_string)
        };
        if let Some(real) = real_ctx {
            req.context_token = Some(real);
        }
    }

    // Ensure base_info is set
    if req.base_info.is_none() {
        req.base_info = Some(BaseInfo::default());
    }

    match state.upstream.get_config(req).await {
        Ok(resp) => Json(resp),
        Err(e) => Json(GetConfigResponse {
            ret: Some(500),
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
    let Some(vtoken) = extract_vtoken(&headers) else {
        return Json(GetUploadUrlResponse {
            ret: 401,
            upload_url: None,
            media_id: None,
            errmsg: Some("Missing Authorization".to_string()),
        });
    };
    {
        let registry = state.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "getuploadurl rejected: unknown virtual token");
            return Json(GetUploadUrlResponse {
                ret: 401,
                upload_url: None,
                media_id: None,
                errmsg: Some(UNKNOWN_VTOKEN_MSG.to_string()),
            });
        }
    }

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

#[derive(Debug, Deserialize)]
pub struct AdminUpdateClientRequest {
    pub name: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AdminClientMutationResponse {
    pub ret: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

pub type AdminDeleteClientResponse = AdminClientMutationResponse;

pub async fn admin_delete_client(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> (StatusCode, Json<AdminDeleteClientResponse>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AdminDeleteClientResponse {
                ret: 401,
                errmsg: Some("Unauthorized".to_string()),
                name: None,
            }),
        );
    }

    let name = name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminDeleteClientResponse {
                ret: 400,
                errmsg: Some("Client name is required".to_string()),
                name: None,
            }),
        );
    }

    match crate::server::pairing::unregister_client_in_hub(state.as_ref(), name).await {
        Ok(()) => (
            StatusCode::OK,
            Json(AdminDeleteClientResponse {
                ret: 0,
                errmsg: None,
                name: Some(name.to_string()),
            }),
        ),
        Err(crate::server::pairing::UnregisterClientError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(AdminDeleteClientResponse {
                ret: 404,
                errmsg: Some(format!("Client `{name}` not found")),
                name: None,
            }),
        ),
        Err(crate::server::pairing::UnregisterClientError::StillOnline) => (
            StatusCode::CONFLICT,
            Json(AdminDeleteClientResponse {
                ret: 409,
                errmsg: Some(format!(
                    "Client `{name}` is still online; stop the backend process first"
                )),
                name: None,
            }),
        ),
        Err(crate::server::pairing::UnregisterClientError::Store(e)) => {
            error!(error = %e, %name, "failed to delete client from store");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminDeleteClientResponse {
                    ret: 500,
                    errmsg: Some("Failed to delete client".to_string()),
                    name: None,
                }),
            )
        }
    }
}

pub async fn admin_update_client(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Path(old_name): axum::extract::Path<String>,
    Json(req): Json<AdminUpdateClientRequest>,
) -> (StatusCode, Json<AdminClientMutationResponse>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AdminClientMutationResponse {
                ret: 401,
                errmsg: Some("Unauthorized".to_string()),
                name: None,
            }),
        );
    }

    let old_name = old_name.trim();
    if old_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminClientMutationResponse {
                ret: 400,
                errmsg: Some("Client name is required".to_string()),
                name: None,
            }),
        );
    }

    match crate::server::pairing::update_client_in_hub(
        state.as_ref(),
        old_name,
        &req.name,
        req.label,
    )
    .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(AdminClientMutationResponse {
                ret: 0,
                errmsg: None,
                name: Some(req.name.trim().to_string()),
            }),
        ),
        Err(crate::server::pairing::UpdateClientError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(AdminClientMutationResponse {
                ret: 404,
                errmsg: Some(format!("Client `{old_name}` not found")),
                name: None,
            }),
        ),
        Err(crate::server::pairing::UpdateClientError::NameTaken) => (
            StatusCode::CONFLICT,
            Json(AdminClientMutationResponse {
                ret: 409,
                errmsg: Some(format!(
                    "Client name `{}` is already taken",
                    req.name.trim()
                )),
                name: None,
            }),
        ),
        Err(crate::server::pairing::UpdateClientError::InvalidName) => (
            StatusCode::BAD_REQUEST,
            Json(AdminClientMutationResponse {
                ret: 400,
                errmsg: Some("Client name cannot be empty".to_string()),
                name: None,
            }),
        ),
        Err(crate::server::pairing::UpdateClientError::Store(e)) => {
            error!(error = %e, %old_name, "failed to update client in store");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminClientMutationResponse {
                    ret: 500,
                    errmsg: Some("Failed to update client".to_string()),
                    name: None,
                }),
            )
        }
    }
}

// ─── iLink status + QR re-login (Admin) ──────────────────────────────────────

#[derive(Serialize)]
pub struct IlinkStatusResponse {
    pub status: &'static str,
    pub code: u8,
}

pub async fn admin_ilink_status(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<IlinkStatusResponse>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(IlinkStatusResponse { status: "unauthorized", code: 0 }),
        );
    }
    let code = state.ilink_status.load(Ordering::Relaxed);
    let status = match code {
        crate::hub::ilink_status::CONNECTED => "connected",
        crate::hub::ilink_status::NEEDS_LOGIN => "needs_login",
        crate::hub::ilink_status::LOGGING_IN => "logging_in",
        _ => "unknown",
    };
    (StatusCode::OK, Json(IlinkStatusResponse { status, code }))
}

pub async fn admin_ilink_relogin(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    if !check_admin_auth(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        );
    }
    let _ = state.relogin_tx.send(());
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

pub async fn admin_ilink_qr_stream(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    // EventSource can't set headers — accept token via query param as fallback.
    // When a token is configured, also accept it as `?token=` query param.
    // When no token is configured, apply the same insecure-flag gate as other admin routes.
    let authed = check_admin_auth(&headers) || admin_token().map_or(insecure_no_auth(), |required| {
        params.get("token").map(String::as_str).unwrap_or("") == required
    });
    if !authed {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Grab the cached Ready event before subscribing, so we don't miss it.
    let cached = state.qr_last_ready.lock().await.clone();
    let rx = state.qr_tx.subscribe();

    let s = stream::unfold((cached, rx), |(cached, mut rx)| async move {
        // Replay cached Ready event on first poll if present.
        if let Some(evt) = cached {
            let data = serde_json::to_string(&evt).unwrap_or_default();
            return Some((Ok(Event::default().data(data)), (None, rx)));
        }
        match rx.recv().await {
            Ok(evt) => {
                let data = serde_json::to_string(&evt).unwrap_or_default();
                Some((Ok(Event::default().data(data)), (None, rx)))
            }
            Err(_) => None,
        }
    });
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
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
    let upstream_user_messages = state.metrics.upstream_user_messages.load(Ordering::Relaxed);
    let upstream_polls_ok = state.upstream.polls_ok.load(Ordering::Relaxed);
    let upstream_polls_err = state.upstream.polls_err.load(Ordering::Relaxed);

    let mut out = String::with_capacity(1024);

    out.push_str("# HELP ilink_hub_clients_online Number of online clients\n");
    out.push_str("# TYPE ilink_hub_clients_online gauge\n");
    out.push_str(&format!("ilink_hub_clients_online {}\n", online));

    out.push_str("# HELP ilink_hub_clients_total Total registered clients\n");
    out.push_str("# TYPE ilink_hub_clients_total gauge\n");
    out.push_str(&format!("ilink_hub_clients_total {}\n", total));

    out.push_str("# HELP ilink_hub_messages_dispatched_total Messages dispatched\n");
    out.push_str("# TYPE ilink_hub_messages_dispatched_total counter\n");
    out.push_str(&format!(
        "ilink_hub_messages_dispatched_total {}\n",
        messages_dispatched
    ));

    out.push_str("# HELP ilink_hub_messages_dropped_total Messages dropped\n");
    out.push_str("# TYPE ilink_hub_messages_dropped_total counter\n");
    out.push_str(&format!(
        "ilink_hub_messages_dropped_total {}\n",
        messages_dropped
    ));

    out.push_str("# HELP ilink_hub_upstream_user_messages_total User-side messages received from upstream (excl. bot echo copies)\n");
    out.push_str("# TYPE ilink_hub_upstream_user_messages_total counter\n");
    out.push_str(&format!(
        "ilink_hub_upstream_user_messages_total {}\n",
        upstream_user_messages
    ));

    out.push_str("# HELP ilink_hub_upstream_polls_ok_total Successful upstream polls\n");
    out.push_str("# TYPE ilink_hub_upstream_polls_ok_total counter\n");
    out.push_str(&format!(
        "ilink_hub_upstream_polls_ok_total {}\n",
        upstream_polls_ok
    ));

    out.push_str("# HELP ilink_hub_upstream_polls_err_total Failed upstream polls\n");
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

/// Wait for a queue notification or hub shutdown, whichever comes first.
async fn wait_notify_or_shutdown(
    queue: &dyn MessageQueue,
    shutdown: &mut watch::Receiver<bool>,
    vtoken: &str,
    poll_secs: u64,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }

    tokio::select! {
        biased;
        _ = wait_shutdown_signal(shutdown) => false,
        notified = async {
            queue.wait_notify(vtoken, poll_secs).await.unwrap_or_else(|e| {
                error!(error = %e, vtoken = %redact_token(&vtoken), "wait_notify failed");
                false
            })
        } => notified,
    }
}

async fn wait_shutdown_signal(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod shutdown_poll_tests {
    use super::{wait_notify_or_shutdown, wait_shutdown_signal};
    use crate::hub::queue::InMemoryQueue;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::watch;

    #[tokio::test]
    async fn wait_notify_or_shutdown_returns_when_shutdown_signaled() {
        let queue = Arc::new(InMemoryQueue::new());
        let (tx, rx) = watch::channel(false);
        let mut shutdown_rx = rx.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
        });

        let start = Instant::now();
        let notified = wait_notify_or_shutdown(queue.as_ref(), &mut shutdown_rx, "v1", 30).await;
        handle.await.unwrap();

        assert!(!notified);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "expected fast return on shutdown, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_shutdown_signal_returns_immediately_when_already_shutting_down() {
        let (_tx, rx) = watch::channel(true);
        let mut shutdown_rx = rx;

        let start = Instant::now();
        wait_shutdown_signal(&mut shutdown_rx).await;

        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
