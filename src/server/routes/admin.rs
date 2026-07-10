//! Hub admin HTTP routes (clients, sessions, iLink status, UI).
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
use tracing::error;

use super::auth::{check_admin_auth, AdminGuard};
use crate::hub::HubState;

// ─── Admin: list clients ──────────────────────────────────────────────────────

pub async fn admin_clients(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let registry = state.clients.registry.read().await;
    let clients: Vec<_> = registry
        .all_clients()
        .iter()
        .map(|c| {
            // Redact vtoken: expose only the first 8 chars so the list is usable
            // for identification while preventing full-token leakage via logs/dashboards.
            let prefix: String = c.vtoken.chars().take(8).collect();
            let redacted = if c.vtoken.chars().count() > 8 {
                format!("{prefix}…")
            } else {
                "…".to_string()
            };
            serde_json::json!({
                "name": c.name,
                "label": c.label,
                "online": c.online,
                "vtoken": redacted,
                "persona_name": c.persona_name,
                "persona_emoji": c.persona_emoji,
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
    pub persona_name: Option<String>,
    pub persona_emoji: Option<String>,
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

#[derive(Debug, Deserialize, Default)]
pub struct AdminDeleteClientQuery {
    /// When `true`, skip the "still online" guard and force-remove the client.
    /// Intended for the bridge manager, which has just killed the child process and
    /// knows the client will stop polling momentarily.
    #[serde(default)]
    pub force: bool,
}

pub async fn admin_delete_client(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AdminDeleteClientQuery>,
) -> (StatusCode, Json<AdminDeleteClientResponse>) {
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

    match crate::server::pairing::unregister_client_in_hub(state.as_ref(), name, query.force).await
    {
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
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
    axum::extract::Path(old_name): axum::extract::Path<String>,
    Json(req): Json<AdminUpdateClientRequest>,
) -> (StatusCode, Json<AdminClientMutationResponse>) {
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
        req.persona_name,
        req.persona_emoji,
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
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
) -> (StatusCode, Json<IlinkStatusResponse>) {
    let code = state.ilink.ilink_status.load(Ordering::Relaxed);
    let status = crate::hub::ilink_status::as_str(code);
    (StatusCode::OK, Json(IlinkStatusResponse { status, code }))
}

pub async fn admin_ilink_relogin(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ = state.ilink.relogin_tx.send(());
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

/// Mint a single-use, short-lived ticket for opening the QR SSE stream.
///
/// Authenticated the normal way (Bearer header), so the long-lived admin token
/// never has to travel in a URL. The browser redeems the returned ticket via
/// `GET /hub/ilink/qr-stream?ticket=<ticket>`.
pub async fn admin_ilink_qr_stream_ticket(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ticket = state.ilink.qr_ticket.issue();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ticket": ticket,
            "expires_in_secs": crate::server::sse_ticket::TICKET_TTL.as_secs(),
        })),
    )
}

pub async fn admin_ilink_qr_stream(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>,
    StatusCode,
> {
    // `EventSource` cannot attach an Authorization header, so the browser opens
    // this stream with a single-use `?ticket=` minted by the (header-authed)
    // `/hub/ilink/qr-stream-ticket` endpoint. The ticket is high-entropy,
    // expires in seconds and is consumed on first use, so leaking it via proxy
    // logs / history is harmless — unlike the raw admin token. Non-browser
    // callers can still authenticate directly with a Bearer header.
    let authed = check_admin_auth(&state.admin, &headers)
        || params
            .get("ticket")
            .is_some_and(|t| state.ilink.qr_ticket.consume(t));
    if !authed {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Grab the cached Ready event before subscribing, so we don't miss it.
    // The mutex is synchronous (`std::sync::Mutex`) and the critical section
    // is just an `Option::clone`, so we never hold it across an `.await`.
    let cached = state
        .ilink
        .qr_last_ready
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let rx = state.ilink.qr_tx.subscribe();

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

// ─── Admin: client sessions list ─────────────────────────────────────────────

pub async fn admin_client_sessions(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let vtoken = {
        let registry = state.clients.registry.read().await;
        registry.get_by_name(name.trim()).map(|c| c.vtoken.clone())
    };
    let Some(vtoken) = vtoken else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ret": 404, "errmsg": "Client not found"})),
        );
    };

    match state
        .store
        .get_all_session_entries_per_vtoken(std::slice::from_ref(&vtoken))
        .await
    {
        Ok(mut map) => {
            let entries = map.remove(&vtoken).unwrap_or_default();
            let sessions: Vec<_> = entries
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.session_name,
                        "last_user_content": e.last_user_content,
                        "waiting_for_reply": e.waiting_for_reply,
                        "user_msg_created_at": e.user_msg_created_at,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!({"ret": 0, "sessions": sessions})),
            )
        }
        Err(e) => {
            error!(error = %e, client = %name, "failed to list sessions");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ret": 500, "errmsg": "Failed to list sessions"})),
            )
        }
    }
}

// ─── Admin: client session history ───────────────────────────────────────────

#[derive(Debug, serde::Deserialize, Default)]
pub struct AdminHistoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn admin_client_session_history(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
    axum::extract::Path((name, session_name)): axum::extract::Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<AdminHistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let vtoken = {
        let registry = state.clients.registry.read().await;
        registry.get_by_name(name.trim()).map(|c| c.vtoken.clone())
    };
    let Some(vtoken) = vtoken else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ret": 404, "errmsg": "Client not found"})),
        );
    };

    let limit = query.limit.unwrap_or(100);
    match state
        .store
        .list_messages_for_session(&vtoken, &session_name, limit)
        .await
    {
        Ok(rows) => {
            let messages: Vec<_> = rows
                .into_iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "role": m.role,
                        "content": m.content,
                        "created_at": m.created_at,
                        "peer_user_id": m.peer_user_id,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!({"ret": 0, "messages": messages})),
            )
        }
        Err(e) => {
            error!(error = %e, client = %name, session = %session_name, "failed to fetch history");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ret": 500, "errmsg": "Failed to fetch history"})),
            )
        }
    }
}

// ─── Web Admin UI ─────────────────────────────────────────────────────────────

static ADMIN_HTML: &str = include_str!("../admin.html");

pub async fn admin_ui() -> axum::response::Html<&'static str> {
    axum::response::Html(ADMIN_HTML)
}
