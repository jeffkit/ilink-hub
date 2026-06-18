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

use crate::hub::{HubState, MessageQueue, MAX_CONCURRENT_POLLS_PER_VTOKEN};
use crate::ilink::types::*;
use crate::server::pairing::register_client_in_hub;

/// Returned when a downstream `Authorization` vtoken is not in the Hub registry.
pub const UNKNOWN_VTOKEN_MSG: &str = "Unknown or revoked virtual token; register via POST /hub/register or ilink-hub-bridge --force-register";

// ─── Auth helper ─────────────────────────────────────────────────────────────

use crate::redact_token;

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
        use subtle::ConstantTimeEq;
        return provided.as_bytes().ct_eq(required.as_bytes()).unwrap_u8() == 1;
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

const MAX_CLIENT_NAME_LEN: usize = 64;

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

    let name = req.name.trim();
    if name.is_empty() || name.len() > MAX_CLIENT_NAME_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(RegisterResponse {
                ret: 400,
                vtoken: String::new(),
                base_url: String::new(),
                errmsg: Some(format!("name must be 1–{MAX_CLIENT_NAME_LEN} characters")),
            }),
        );
    }
    if let Some(label) = &req.label {
        if label.len() > MAX_CLIENT_NAME_LEN {
            return (
                StatusCode::BAD_REQUEST,
                Json(RegisterResponse {
                    ret: 400,
                    vtoken: String::new(),
                    base_url: String::new(),
                    errmsg: Some(format!(
                        "label must be at most {MAX_CLIENT_NAME_LEN} characters"
                    )),
                }),
            );
        }
    }

    let (vtoken, _is_new) =
        register_client_in_hub(state.as_ref(), req.name.clone(), req.label.clone()).await;

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

    // SEC-003: gate concurrent long-polls per vtoken BEFORE doing any
    // registry work. The tracker increments the count and returns a guard;
    // dropping the guard decrements (via Drop) regardless of whether we
    // ultimately serve the long-poll or bail with 429. A poisoned counts
    // mutex returns count=0 (F-M2-2) — we then fall through to the
    // 429-without-guard path which is always safe.
    let (concurrent_polls, poll_guard) = state.clients.poll_tracker.enter(&vtoken);
    if concurrent_polls > MAX_CONCURRENT_POLLS_PER_VTOKEN {
        // Over the per-vtoken cap — drop the guard so the count returns to
        // MAX, then reject.  We do NOT proceed to mark_seen / wait_notify:
        // the request is over-budget and may indicate an attacker (or a
        // misconfigured bridge) trying to exhaust Hub resources.
        warn!(
            vtoken = %redact_token(&vtoken),
            concurrent = concurrent_polls,
            cap = MAX_CONCURRENT_POLLS_PER_VTOKEN,
            "getupdates rejected: too many concurrent long-polls for this vtoken"
        );
        drop(poll_guard);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(GetUpdatesResponse {
                ret: Some(429),
                errcode: None,
                errmsg: Some("too many concurrent polls for this vtoken".to_string()),
                msgs: None,
                get_updates_buf: None,
            }),
        );
    }

    // Split-brain detection: more than one (but still under the cap)
    // concurrent long-poll means two bridge processes share one
    // credential/token and will compete for this vtoken's queue (drain is
    // a destructive read), so inbound messages get stolen
    // non-deterministically.  Anything strictly above MAX has already been
    // rejected above; we only warn for the legal-but-suspicious 1 < n <= MAX
    // range.
    if concurrent_polls > 1 {
        warn!(
            vtoken = %redact_token(&vtoken),
            concurrent = concurrent_polls,
            "multiple bridges are long-polling the same vtoken — they share one credential/token \
             and will steal each other's messages. Give each backend its own registration \
             instead of reusing a token."
        );
    }

    {
        // F-M2-1: collapse the read (existence) + write (mark_seen) into a
        // single write guard. Holding two separate locks creates a stale-
        // online window where evict_stale could flip `online=false` between
        // the read and the mark_seen. With one guard, the vtoken existence
        // check and the last-seen timestamp bump are atomic w.r.t. evict_stale.
        let mut registry = state.clients.registry.write().await;
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
        registry.mark_seen(&vtoken);
    }

    // Use timeout from legacy field if provided, otherwise 30s
    let poll_secs = req.timeout.unwrap_or(30).min(60) as u64;
    let mut shutdown_rx = state.ilink.shutdown.clone();
    let notified = wait_notify_or_shutdown(
        state.clients.queue.as_ref(),
        &mut shutdown_rx,
        &vtoken,
        poll_secs,
    )
    .await;
    if !notified && *state.ilink.shutdown.borrow() {
        debug!(vtoken = %redact_token(&vtoken), "getupdates returning early due to shutdown");
    }

    let messages = state
        .clients
        .queue
        .drain(&vtoken)
        .await
        .unwrap_or_else(|e| {
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

#[tracing::instrument(
    skip_all,
    fields(
        vtoken = tracing::field::Empty,
        vctx   = tracing::field::Empty,
    )
)]
pub async fn sendmessage(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(mut req): Json<SendMessageRequest>,
) -> Json<SendMessageResponse> {
    state
        .metrics
        .sendmessage_total
        .fetch_add(1, Ordering::Relaxed);

    let Some(vtoken) = extract_vtoken(&headers) else {
        state
            .metrics
            .sendmessage_errors
            .fetch_add(1, Ordering::Relaxed);
        return Json(SendMessageResponse::err(
            401,
            "Missing Authorization header",
        ));
    };
    tracing::Span::current().record("vtoken", redact_token(&vtoken));

    {
        let registry = state.clients.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "sendmessage rejected: unknown virtual token");
            state
                .metrics
                .sendmessage_errors
                .fetch_add(1, Ordering::Relaxed);
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

    tracing::Span::current().record("vctx", &vctx);

    // Translate virtual → real context token + get peer_user_id (memory first, DB fallback)
    let (real_ctx, peer_user_id) = state
        .routing
        .ctx_map
        .resolve_full(&vctx)
        .unwrap_or_else(|| ("".to_string(), "".to_string()));

    let (real_ctx, peer_user_id) = if real_ctx.is_empty() {
        let db_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.store.resolve_context_token_full(&vctx),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("context_token DB lookup timed out")));
        match db_result {
            Ok(Some((r, p))) => {
                state
                    .routing
                    .ctx_map
                    .seed_full(vctx.clone(), r.clone(), p.clone());
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

    let mut active_session: Option<String> = None;
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

        // Pre-fetch active session once; reused for both cli_session_id persistence and
        // the outbound-origin footer, avoiding two identical DB queries per message.
        active_session = match replied_session_name.clone() {
            Some(n) => Some(n),
            None => tokio::time::timeout(
                std::time::Duration::from_secs(5),
                state.store.get_active_session_name(&vctx, &vtoken),
            )
            .await
            .unwrap_or_else(|_| {
                warn!(vctx = %vctx, "get_active_session_name timed out");
                Err(anyhow::anyhow!("timed out"))
            })
            .ok(),
        };

        // Read cli_session_id from hub_ext and persist it to the correct session.
        if let Some(ext) = msg.ilink_hub_ext.as_mut() {
            if let Some(cli_sid) = ext.cli_session_id.take() {
                let t = cli_sid.trim().to_string();
                if !t.is_empty() {
                    let session_name = active_session
                        .clone()
                        .unwrap_or_else(|| "default".to_string());
                    let set_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        state
                            .store
                            .set_backend_session(&vctx, &vtoken, &session_name, &t),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("set_backend_session timed out")));
                    if let Err(e) = set_result {
                        warn!(error = %e, vctx = %vctx, "failed to persist backend session");
                    }
                }
            }
        }
        // Strip ilink_hub_ext before forwarding to upstream iLink.
        msg.ilink_hub_ext = None;

        // Conversation scope for the quote index (captured before peer_user_id is moved).
        let conv_scope = peer_user_id.clone();
        // Replace virtual context_token with real one and inject to_user_id if missing
        msg.context_token = Some(real_ctx);
        if msg.to_user_id.is_none() && !peer_user_id.is_empty() {
            msg.to_user_id = Some(peer_user_id.clone());
        }
        msg.ensure_outbound();

        // Session-persist-only messages (empty body, cli_session_id already persisted above):
        // return early BEFORE appending the footer. Without this early check, the footer text
        // itself would make the message appear non-empty, bypassing the guard below and causing
        // an empty-looking message (containing only the footer) to be forwarded to iLink.
        //
        // Media messages (image/file/video) have no text but do have content — allow them through.
        let is_text_empty = msg.text().map(|t| t.trim().is_empty()).unwrap_or(true);
        if is_text_empty && !msg.has_content() {
            return Json(SendMessageResponse::default());
        }

        let (client_meta, registered_count) = {
            let reg = state.clients.registry.read().await;
            (
                reg.get_by_vtoken(&vtoken)
                    .map(|i| (i.name.clone(), i.label.clone())),
                reg.online_clients().len(),
            )
        };

        // active_session already resolved above — no second DB query needed.

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

        // Index this outbound reply so a later quote-reply in the same conversation routes
        // back to this backend + session. Content-based by necessity: real iLink never echoes
        // bot messages and strips `msg_id` from the quoted `ref_msg`, leaving only the text.
        let outbound_text = msg.text().map(str::to_string);
        if let (Some((name, label)), Some(text)) = (client_meta, outbound_text) {
            let origin = crate::hub::quote_route::QuoteOrigin::Client {
                vtoken: vtoken.clone(),
                name,
                label,
                session_name: active_session.clone(),
            };
            state
                .routing
                .quote_index
                .lock()
                .await
                .register_outbound_content(&conv_scope, &text, origin);
        }
    }
    // When the bridge sends a session-persist-only message (empty body, cli_session_id already
    // consumed above), skip forwarding to iLink — an empty text message would be rejected or
    // would confuse the user. The only side-effect we needed (session UUID persistence) already
    // happened via set_backend_session above.
    // Media messages (image/file/video) have no text but do carry content — pass them through.
    let has_text = req
        .msg
        .as_ref()
        .map(|m| !m.text().unwrap_or("").trim().is_empty())
        .unwrap_or(false);
    let has_media = req.msg.as_ref().map(|m| m.has_content()).unwrap_or(false);
    if !has_text && !has_media {
        return Json(SendMessageResponse::default());
    }

    // Fire-and-forget: record assistant reply to history (only non-empty, non-partial messages).
    let is_partial = req
        .msg
        .as_ref()
        .and_then(|m| m.message_state)
        .map(|s| s != crate::ilink::types::message_state::FINISH)
        .unwrap_or(false);
    if !is_partial {
        if let Some(content) = req.msg.as_ref().and_then(|m| m.text()).map(str::to_string) {
            let session_name = active_session.as_deref().unwrap_or("default").to_string();
            let store = state.store.clone();
            let (vctx4, vtoken4, peer4) = (vctx.clone(), vtoken.clone(), peer_user_id.clone());
            let sem = state.persist_sem.clone();
            tokio::spawn(async move {
                let Ok(_permit) = sem.try_acquire() else {
                    return;
                };
                if let Err(e) = store
                    .save_message(
                        &vctx4,
                        Some(&vtoken4),
                        &session_name,
                        &peer4,
                        "assistant",
                        &content,
                    )
                    .await
                {
                    warn!(error = %e, "failed to save assistant message to history");
                }
            });
        }
    }

    if req.base_info.is_none() {
        req.base_info = Some(BaseInfo::default());
    }

    match state.ilink.upstream.send_message(req).await {
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
        let registry = state.clients.registry.read().await;
        if registry.get_by_vtoken(&vtoken).is_none() {
            warn!(vtoken = %redact_token(&vtoken), "sendtyping rejected: unknown virtual token");
            return Json(serde_json::json!({"ret": 401, "errmsg": UNKNOWN_VTOKEN_MSG}));
        }
    }

    match state.ilink.upstream.send_typing(req).await {
        Ok(_) => Json(serde_json::json!({"ret": 0})),
        Err(e) => Json(serde_json::json!({
            "ret": 500,
            "errmsg": format!("upstream error: {e}")
        })),
    }
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
        let registry = state.clients.registry.read().await;
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
        let real_ctx = state.routing.ctx_map.resolve(vctx);
        if let Some(real) = real_ctx {
            req.context_token = Some(real);
        }
    }

    // Ensure base_info is set
    if req.base_info.is_none() {
        req.base_info = Some(BaseInfo::default());
    }

    match state.ilink.upstream.get_config(req).await {
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
        let registry = state.clients.registry.read().await;
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

    match state.ilink.upstream.get_upload_url(req).await {
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

#[derive(Debug, Deserialize, Default)]
pub struct AdminDeleteClientQuery {
    /// When `true`, skip the "still online" guard and force-remove the client.
    /// Intended for the bridge manager, which has just killed the child process and
    /// knows the client will stop polling momentarily.
    #[serde(default)]
    pub force: bool,
}

pub async fn admin_delete_client(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AdminDeleteClientQuery>,
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
            Json(IlinkStatusResponse {
                status: "unauthorized",
                code: 0,
            }),
        );
    }
    let code = state.ilink.ilink_status.load(Ordering::Relaxed);
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
    let _ = state.ilink.relogin_tx.send(());
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

pub async fn admin_ilink_qr_stream(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>,
    StatusCode,
> {
    // EventSource can't set custom headers — accept token via ?token= query param as fallback.
    // Trade-off: the full URL (including the token) appears in reverse-proxy access logs and
    // browser history. Operators should configure their proxy to redact or omit the ?token=
    // query parameter from access logs for this endpoint.
    let authed = check_admin_auth(&headers)
        || admin_token().map_or(insecure_no_auth(), |required| {
            use subtle::ConstantTimeEq;
            let provided = params.get("token").map(String::as_str).unwrap_or("");
            provided.as_bytes().ct_eq(required.as_bytes()).unwrap_u8() == 1
        });
    if !authed {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Grab the cached Ready event before subscribing, so we don't miss it.
    let cached = state.ilink.qr_last_ready.lock().await.clone();
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

// ─── Web Admin UI ─────────────────────────────────────────────────────────────

static ADMIN_HTML: &str = include_str!("admin.html");

pub async fn admin_ui() -> axum::response::Html<&'static str> {
    axum::response::Html(ADMIN_HTML)
}

// ─── Metrics (Prometheus text format) ────────────────────────────────────────

pub async fn metrics(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    if !check_admin_auth(&headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }

    let (online, total, client_names_by_vtoken) = {
        let registry = state.clients.registry.read().await;
        let online = registry.online_clients().len() as u64;
        let total = registry.all_clients().len() as u64;
        let names: std::collections::HashMap<String, String> = registry
            .all_clients()
            .iter()
            .map(|c| (c.vtoken.clone(), c.name.clone()))
            .collect();
        (online, total, names)
    };

    let queue_sizes = state.clients.queue.queue_sizes().await.unwrap_or_else(|e| {
        error!(error = %e, "queue_sizes failed");
        std::collections::HashMap::new()
    });

    let messages_dispatched = state.metrics.messages_dispatched.load(Ordering::Relaxed);
    let messages_dropped = state.metrics.messages_dropped.load(Ordering::Relaxed);
    let upstream_user_messages = state.metrics.upstream_user_messages.load(Ordering::Relaxed);
    let sendmessage_total = state.metrics.sendmessage_total.load(Ordering::Relaxed);
    let sendmessage_errors = state.metrics.sendmessage_errors.load(Ordering::Relaxed);
    let upstream_polls_ok = state.ilink.upstream.polls_ok();
    let upstream_polls_err = state.ilink.upstream.polls_err();
    let relogin_attempts = state.ilink.upstream.relogin_attempts();
    let ilink_status = state.ilink.ilink_status.load(Ordering::Relaxed);
    let ctx_map_size = state.routing.ctx_map.len();
    let dispatcher_lagged = state.metrics.dispatcher_lagged.load(Ordering::Relaxed);

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

    out.push_str(
        "# HELP ilink_hub_sendmessage_total Total sendmessage calls from backend clients\n",
    );
    out.push_str("# TYPE ilink_hub_sendmessage_total counter\n");
    out.push_str(&format!(
        "ilink_hub_sendmessage_total {}\n",
        sendmessage_total
    ));

    out.push_str("# HELP ilink_hub_sendmessage_errors_total sendmessage calls rejected (unknown token, missing context, etc.)\n");
    out.push_str("# TYPE ilink_hub_sendmessage_errors_total counter\n");
    out.push_str(&format!(
        "ilink_hub_sendmessage_errors_total {}\n",
        sendmessage_errors
    ));

    out.push_str("# HELP ilink_hub_dispatcher_lagged_total Number of messages missed because the dispatcher lagged behind the broadcast channel\n");
    out.push_str("# TYPE ilink_hub_dispatcher_lagged_total counter\n");
    out.push_str(&format!(
        "ilink_hub_dispatcher_lagged_total {}\n",
        dispatcher_lagged
    ));

    out.push_str("# HELP ilink_hub_relogin_attempts_total Number of QR re-login attempts (manual or automatic)\n");
    out.push_str("# TYPE ilink_hub_relogin_attempts_total counter\n");
    out.push_str(&format!(
        "ilink_hub_relogin_attempts_total {}\n",
        relogin_attempts
    ));

    out.push_str("# HELP ilink_hub_ilink_status iLink upstream connection status (0=unknown 1=connected 2=needs_login 3=logging_in)\n");
    out.push_str("# TYPE ilink_hub_ilink_status gauge\n");
    out.push_str(&format!("ilink_hub_ilink_status {}\n", ilink_status));

    out.push_str(
        "# HELP ilink_hub_ctx_map_size Number of virtual context token entries in memory cache\n",
    );
    out.push_str("# TYPE ilink_hub_ctx_map_size gauge\n");
    out.push_str(&format!("ilink_hub_ctx_map_size {}\n", ctx_map_size));

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
                error!(error = %e, vtoken = %redact_token(vtoken), "wait_notify failed");
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

#[cfg(test)]
mod admin_auth_tests {
    use super::*;
    use axum::http::HeaderMap;

    #[tokio::test]
    async fn test_check_admin_auth_wrong_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token-here".parse().unwrap());
        assert!(!check_admin_auth(&headers));
    }

    #[tokio::test]
    async fn test_check_admin_auth_empty_headers() {
        let headers = HeaderMap::new();
        let is_insecure = insecure_no_auth();
        assert_eq!(check_admin_auth(&headers), is_insecure);
    }
}
