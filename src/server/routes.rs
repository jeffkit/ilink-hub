//! iLink-compatible HTTP routes exposed to backend clients.
//! Clients configure `base_url = https://your-hub.example.com` and
//! use their virtual token — they see the same API as ilinkai.weixin.qq.com.

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, HeaderMap, StatusCode},
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

// ─── Auth helpers ─────────────────────────────────────────────────────────────

use crate::redact_token;

/// Schema check for Hub-issued virtual tokens. Tokens are minted in
/// `hub::registry::ClientInfo::new` as `vhub_{uuid v4 simple}` (32 lowercase
/// hex chars). Reject anything that does not match before doing registry work,
/// so a misconfigured client cannot inject iLink-style bot tokens
/// (`botid@im.bot:secret`) into the vtoken lookup path.
pub(crate) fn is_valid_vtoken(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("vhub_") else {
        return false;
    };
    rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Public re-export for the MCP router.
pub fn extract_vtoken_pub(headers: &axum::http::HeaderMap) -> Option<String> {
    extract_vtoken(headers)
}

fn extract_vtoken(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| is_valid_vtoken(s))
        .map(crate::hub::hash_vtoken)
}

fn check_admin_auth(admin: &crate::hub::AdminConfig, headers: &HeaderMap) -> bool {
    if let Some(required) = &admin.token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        use subtle::ConstantTimeEq;
        return provided.as_bytes().ct_eq(required.as_bytes()).unwrap_u8() == 1;
    }
    admin.insecure_no_auth
}

/// Axum extractor that enforces admin authentication. Any route that extracts
/// `AdminGuard` is automatically protected — no per-handler `check_admin_auth`
/// call needed. New admin routes added in the future cannot forget auth.
pub struct AdminGuard;

impl FromRequestParts<Arc<HubState>> for AdminGuard {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<HubState>,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        if check_admin_auth(&state.admin, headers) {
            Ok(AdminGuard)
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            ))
        }
    }
}

// ─── Registration (Hub-specific, non-iLink) ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub label: Option<String>,
    /// Optional description of the Agent's capabilities.
    /// Exposed via the MCP `list_agents` tool so other Agents can understand
    /// what this Agent can do before calling it.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional display name shown in `/list` and prepended to replies.
    /// Defaults to the client name when omitted on the bridge side.
    #[serde(default)]
    pub persona_name: Option<String>,
    /// Optional emoji avatar shown alongside `persona_name` in `/list` and replies.
    #[serde(default)]
    pub persona_emoji: Option<String>,
    /// Optional one-line description returned by the MCP `list_agents` tool.
    #[serde(default)]
    pub description: Option<String>,
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
    if !check_admin_auth(&state.admin, &headers) {
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

    let outcome = register_client_in_hub(
        state.as_ref(),
        req.name.clone(),
        req.label.clone(),
        req.description.clone(),
    )
    .await;

    // M1: plaintext is only available for brand-new registrations. When an existing
    // client name is re-registered the original plaintext is irrecoverable (only the
    // SHA-256 hash was ever stored). Return 409 so the bridge can either retry with
    // a different name or use --force-register to replace the old entry.
    if outcome.plaintext.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(RegisterResponse {
                ret: 409,
                vtoken: String::new(),
                base_url: String::new(),
                errmsg: Some(format!(
                    "client name '{}' is already registered; use --force-register to replace it",
                    req.name
                )),
            }),
        );
    }

    // Apply persona fields (if provided) immediately after a successful fresh registration.
    if req.persona_name.is_some() || req.persona_emoji.is_some() {
        {
            let mut registry = state.clients.registry.write().await;
            registry.set_persona(
                &outcome.hashed,
                req.persona_name.clone(),
                req.persona_emoji.clone(),
            );
        }
        if let Err(e) = state
            .store
            .update_client_persona(
                &outcome.hashed,
                req.persona_name.as_deref(),
                req.persona_emoji.as_deref(),
            )
            .await
        {
            warn!(error = %e, name = %req.name, "failed to persist persona on registration");
        }
    }

    // Persist description if provided.
    if let Some(desc) = &req.description {
        if !desc.is_empty() {
            {
                let mut registry = state.clients.registry.write().await;
                registry.set_description(&outcome.hashed, Some(desc.clone()));
            }
            if let Err(e) = state
                .store
                .update_client_description(&outcome.hashed, Some(desc.as_str()))
                .await
            {
                warn!(error = %e, name = %req.name, "failed to persist description on registration");
            }
        }
    }

    (
        StatusCode::OK,
        Json(RegisterResponse {
            ret: 0,
            vtoken: outcome.plaintext.clone(),
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
    // RAII guard: records latency on every return path (success, 401, 429,
    // 503, drain-empty). The guard is dropped as the function returns.
    let _histo = HistoGuard::new(&state.metrics.getupdates_latency_ms);
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
    // registry work. The tracker first enforces a Hub-wide cap (so a single
    // misbehaving vtoken cannot starve the rest of the fleet), then a
    // per-vtoken cap. The Hub-wide cap is lock-free (AtomicUsize fetch_add);
    // the per-vtoken cap uses a StdMutex (poison-safe per F-M2-2).
    let enter = state.clients.poll_tracker.enter(&vtoken);
    let (concurrent_polls, poll_guard) = match enter {
        crate::hub::EnterOutcome::HubLimitReached { total, cap } => {
            // Hub-wide cap reached. Reject with 503 (Service Unavailable) —
            // distinct from the 429 we use for the per-vtoken cap so operators
            // can tell "I'm one of too many clients" (503) from "this single
            // vtoken is double-polling itself" (429).
            warn!(
                vtoken = %redact_token(&vtoken),
                total,
                cap,
                "getupdates rejected: Hub-wide concurrent long-poll cap reached"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(GetUpdatesResponse {
                    ret: Some(503),
                    errcode: None,
                    errmsg: Some(format!(
                        "Hub is at capacity ({total}/{cap} concurrent long-polls). Retry later."
                    )),
                    msgs: None,
                    get_updates_buf: None,
                }),
            );
        }
        crate::hub::EnterOutcome::Poisoned { guard } => {
            // Per-vtoken mutex is poisoned; the counts map is unreliable. We
            // still hold a guard that decrements the Hub-wide total on drop,
            // but we have no usable per-vtoken count. The safest behaviour is
            // to fall through with a sentinel count of 0 (F-M2-2): the
            // per-vtoken 429 gate won't trip, but the Hub-wide gate already
            // passed and the registry check below is still authoritative.
            warn!(
                vtoken = %redact_token(&vtoken),
                "getupdates: per-vtoken poll counter is poisoned; serving without split-brain detection"
            );
            (0usize, guard)
        }
        crate::hub::EnterOutcome::Ok { per_vtoken, guard } => (per_vtoken, guard),
    };
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

    // Existence check + online-flag read under read lock.
    let already_online = {
        let registry = state.clients.registry.read().await;
        match registry.get_by_vtoken(&vtoken) {
            None => {
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
            Some(info) => info.online,
        }
    };

    // Lock-free timestamp bump — no write lock on the hot path.
    {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        state
            .clients
            .last_seen
            .entry(vtoken.clone())
            .or_insert_with(|| std::sync::atomic::AtomicU64::new(0))
            .store(now_secs, std::sync::atomic::Ordering::Relaxed);
    }

    // First contact after registration or after the health checker marked this
    // client offline: take the write lock once to flip online=true.
    if !already_online {
        state.clients.registry.write().await.mark_online(&vtoken);
    }

    // Max poll is 55s — well within the upstream HTTP client's 70s socket timeout,
    // leaving 15s margin for the drain + response serialization path.
    let poll_secs = req.timeout.unwrap_or(30).min(55) as u64;
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

    // Single DB round-trip: resolve real_ctx + peer_user_id + active session name.
    // Replaces two serial queries (resolve_context_token_full + get_active_session_name).
    let (real_ctx, peer_user_id, db_session_name) = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.store.resolve_send_context(&vctx, &vtoken),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("context_token DB lookup timed out")))
    {
        Ok(Some(triple)) => triple,
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

        // Use bridge-echoed session name if present, else fall back to what the DB returned.
        // No second DB query needed — db_session_name was fetched in the combined query above.
        active_session = replied_session_name.clone().or(Some(db_session_name));

        // Read cli_session_id from hub_ext and persist it to the correct session.
        // Also resolve any pending A2A call waiter.
        let mut is_a2a_reply = false;
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

            // A2A: if this reply is the response to a `call_agent` MCP call,
            // resolve the waiter so the caller can proceed. The caller's MCP
            // flow is responsible for delivering the reply to the WeChat user
            // (with @-mention + persona), so we suppress this target's own
            // upstream send to avoid a duplicate.
            if let Some(call_id) = ext.a2a_call_id.take() {
                is_a2a_reply = true;
                if let Some(reply_text) = msg.text().map(str::to_string) {
                    if !call_id.is_empty() && !reply_text.is_empty() {
                        state.a2a_waiter.resolve(&call_id, reply_text);
                    }
                }
            }
        }

        // A2A reply: cli_session_id (if any) was persisted above and the waiter
        // was resolved. Skip footer / quote-index / history / upstream send —
        // the caller's `call_agent` flow is the one that surfaces this reply
        // to the WeChat user.
        if is_a2a_reply {
            return Json(SendMessageResponse::ok());
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
        if is_text_empty && !msg.has_media_content() {
            return Json(SendMessageResponse::default());
        }

        let (client_meta, registered_count) = {
            let reg = state.clients.registry.read().await;
            (
                reg.get_by_vtoken(&vtoken).map(|i| {
                    (
                        i.name.clone(),
                        i.label.clone(),
                        i.persona_name.clone(),
                        i.persona_emoji.clone(),
                    )
                }),
                reg.online_clients().len(),
            )
        };

        // active_session already resolved above — no second DB query needed.

        if crate::hub::should_append_outbound_origin_label(
            registered_count,
            state.admin.outbound_origin_label.as_deref(),
        ) {
            if let Some((ref name, ref label, ref persona_name, ref persona_emoji)) = client_meta {
                crate::hub::apply_persona_and_footer_to_first_text_item(
                    msg,
                    persona_name.as_deref(),
                    persona_emoji.as_deref(),
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
        if let (Some((name, label, _, _)), Some(text)) = (client_meta, outbound_text) {
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
            let metrics = Arc::clone(&state.metrics);
            tokio::spawn(async move {
                let Ok(_permit) = sem.try_acquire() else {
                    metrics
                        .messages_persist_dropped
                        .fetch_add(1, Ordering::Relaxed);
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

    // Histogram the *upstream* round-trip only — the Hub-internal work
    // (context translation, footer append, HubExt) is excluded by placing
    // the guard at the call site, not the handler entry.
    let upstream_start = std::time::Instant::now();
    let result = state.ilink.upstream.send_message(req).await;
    state
        .metrics
        .sendmessage_upstream_latency_ms
        .observe(upstream_start.elapsed());

    match result {
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
        if let Ok(Some(real)) = state.store.resolve_context_token(vctx).await {
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
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AdminDeleteClientQuery>,
) -> (StatusCode, Json<AdminDeleteClientResponse>) {
    if !check_admin_auth(&state.admin, &headers) {
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
    if !check_admin_auth(&state.admin, &headers) {
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
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<IlinkStatusResponse>) {
    if !check_admin_auth(&state.admin, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(IlinkStatusResponse {
                status: "unauthorized",
                code: 0,
            }),
        );
    }
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

static ADMIN_HTML: &str = include_str!("admin.html");

pub async fn admin_ui() -> axum::response::Html<&'static str> {
    axum::response::Html(ADMIN_HTML)
}

// ─── Metrics (Prometheus text format) ────────────────────────────────────────

pub async fn metrics(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    if !check_admin_auth(&state.admin, &headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }

    let _hub_name = std::env::var("HUB_NAME").unwrap_or_else(|_| "default".to_string());

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
    let messages_persist_dropped = state
        .metrics
        .messages_persist_dropped
        .load(Ordering::Relaxed);
    let upstream_user_messages = state.metrics.upstream_user_messages.load(Ordering::Relaxed);
    let sendmessage_total = state.metrics.sendmessage_total.load(Ordering::Relaxed);
    let sendmessage_errors = state.metrics.sendmessage_errors.load(Ordering::Relaxed);
    let upstream_polls_ok = state.ilink.upstream.polls_ok();
    let upstream_polls_err = state.ilink.upstream.polls_err();
    let relogin_attempts = state.ilink.upstream.relogin_attempts();
    let ilink_status = state.ilink.ilink_status.load(Ordering::Relaxed);
    let created = state.metrics.process_start_unix_secs;

    let mut out = String::with_capacity(2048);

    out.push_str("# HELP ilink_hub_clients_online Number of online clients\n");
    out.push_str("# TYPE ilink_hub_clients_online gauge\n");
    out.push_str(&format!("ilink_hub_clients_online {}\n", online));

    out.push_str("# HELP ilink_hub_clients_total Total registered clients\n");
    out.push_str("# TYPE ilink_hub_clients_total gauge\n");
    out.push_str(&format!("ilink_hub_clients_total {}\n", total));

    render_counter(
        &mut out,
        "ilink_hub_messages_dispatched_total",
        "Messages dispatched",
        messages_dispatched,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_messages_dropped_total",
        "Messages dropped",
        messages_dropped,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_messages_persist_dropped_total",
        "Message history persist tasks dropped due to semaphore exhaustion (DB too slow)",
        messages_persist_dropped,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_user_messages_total",
        "User-side messages received from upstream (excl. bot echo copies)",
        upstream_user_messages,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_polls_ok_total",
        "Successful upstream polls",
        upstream_polls_ok,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_polls_err_total",
        "Failed upstream polls",
        upstream_polls_err,
        created,
    );

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

    render_counter(
        &mut out,
        "ilink_hub_sendmessage_total",
        "Total sendmessage calls from backend clients",
        sendmessage_total,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_sendmessage_errors_total",
        "sendmessage calls rejected (unknown token, missing context, etc.)",
        sendmessage_errors,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_relogin_attempts_total",
        "Number of QR re-login attempts (manual or automatic)",
        relogin_attempts,
        created,
    );

    out.push_str("# HELP ilink_hub_ilink_status iLink upstream connection status (0=unknown 1=connected 2=needs_login 3=logging_in)\n");
    out.push_str("# TYPE ilink_hub_ilink_status gauge\n");
    out.push_str(&format!("ilink_hub_ilink_status {}\n", ilink_status));

    let quote_index_ready = state.quote_index_warmed.load(Ordering::Relaxed) as u8;
    out.push_str("# HELP ilink_hub_quote_index_ready 1 if the in-memory quote-reply index has finished warming up from DB, 0 during cold-start window\n");
    out.push_str("# TYPE ilink_hub_quote_index_ready gauge\n");
    out.push_str(&format!(
        "ilink_hub_quote_index_ready {quote_index_ready}\n"
    ));

    // Histograms. We render them in Prometheus text format (cumulative
    // bucket counts, plus `_count`, `_sum`, and `_created` siblings). The bucket layout
    // is defined in [`crate::hub::HISTOGRAM_BUCKETS_MS`].
    render_histogram(
        &mut out,
        "ilink_hub_getupdates_latency_ms",
        "Latency of getupdates long-polls (handler entry to drain), in milliseconds",
        &state.metrics.getupdates_latency_ms,
        created,
    );
    render_histogram(
        &mut out,
        "ilink_hub_sendmessage_upstream_latency_ms",
        "Latency of upstream sendmessage HTTP round-trip, in milliseconds",
        &state.metrics.sendmessage_upstream_latency_ms,
        created,
    );
    render_histogram(
        &mut out,
        "ilink_hub_dispatch_latency_ms",
        "Latency of inbound dispatch pipeline (synchronous portion), in milliseconds",
        &state.metrics.dispatch_latency_ms,
        created,
    );

    (StatusCode::OK, out)
}

/// RAII guard that records a latency observation when dropped. Use at
/// the entry of a hot-path handler so every return path (including
/// early-return 4xx/5xx) is observed. The drop handler is `#[inline]`
/// and lock-free, so the per-request cost is a single `Instant::elapsed`
/// + the histogram's 8-bucket linear scan.
// Alias for readability at call sites; the shared impl lives in hub::LatencyGuard.
type HistoGuard<'a> = crate::hub::LatencyGuard<'a>;

/// Render a single `LatencyHistogram` as a Prometheus text-format block.
/// Emits:
/// - `<name>_bucket{le="N"} <cumulative_count>` for each boundary + `+Inf`
/// - `<name>_count` total observations
/// - `<name>_sum` total observed **milliseconds** (rounded down from the
///   internally-tracked microsecond sum; see N-02 note on
///   `LatencyHistogram::sum_us`)
/// - `<name>_created` process start timestamp (OpenMetrics convention)
fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    h: &crate::hub::LatencyHistogram,
    created: f64,
) {
    use crate::hub::HISTOGRAM_BUCKETS_MS;
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    let mut cumulative: u64 = 0;
    for (i, boundary) in HISTOGRAM_BUCKETS_MS.iter().enumerate() {
        let count = h.buckets[i].load(Ordering::Relaxed);
        cumulative = cumulative.saturating_add(count);
        out.push_str(&format!(
            "{name}_bucket{{le=\"{boundary}\"}} {cumulative}\n"
        ));
    }
    let overflow = h.buckets[HISTOGRAM_BUCKETS_MS.len()].load(Ordering::Relaxed);
    cumulative = cumulative.saturating_add(overflow);
    out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {cumulative}\n"));
    let total = h.count.load(Ordering::Relaxed);
    out.push_str(&format!("{name}_count {total}\n"));
    // sum_us / 1000 keeps the on-the-wire unit (milliseconds) stable for
    // existing Prometheus dashboards while preserving sub-millisecond
    // resolution internally. Sub-millisecond observations now contribute a
    // positive amount after enough observations accumulate (e.g. four
    // 250 μs dispatches contribute 1 to the displayed sum).
    let sum_us = h.sum_us.load(Ordering::Relaxed);
    let sum_ms = sum_us / 1000;
    out.push_str(&format!("{name}_sum {sum_ms}\n"));
    out.push_str(&format!("{name}_created {created}\n"));
}

/// Render a single counter metric in Prometheus text format, including the mandatory
/// `_created` timestamp so scrape tools can compute per-second rates correctly after
/// a process restart (OpenMetrics / Prometheus 2.x `_created` convention).
// `name` must already include the `_total` suffix (Prometheus counter naming convention).
// The `# HELP` and `# TYPE` lines use the base name without `_total` per the spec.
fn render_counter(out: &mut String, name: &str, help: &str, value: u64, created: f64) {
    let base = name.strip_suffix("_total").unwrap_or(name);
    out.push_str(&format!("# HELP {base} {help}\n"));
    out.push_str(&format!("# TYPE {base} counter\n"));
    out.push_str(&format!("{name} {value}\n"));
    out.push_str(&format!("{base}_created {created}\n"));
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
    use crate::hub::AdminConfig;
    use axum::http::HeaderMap;

    #[tokio::test]
    async fn test_check_admin_auth_wrong_token() {
        let admin = AdminConfig {
            token: Some("correct-token".to_string()),
            insecure_no_auth: false,
            outbound_origin_label: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token-here".parse().unwrap());
        assert!(!check_admin_auth(&admin, &headers));
    }

    #[tokio::test]
    async fn test_check_admin_auth_correct_token() {
        let admin = AdminConfig {
            token: Some("correct-token".to_string()),
            insecure_no_auth: false,
            outbound_origin_label: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer correct-token".parse().unwrap());
        assert!(check_admin_auth(&admin, &headers));
    }

    #[tokio::test]
    async fn test_check_admin_auth_empty_headers_no_token_no_insecure() {
        let admin = AdminConfig {
            token: None,
            insecure_no_auth: false,
            outbound_origin_label: None,
        };
        let headers = HeaderMap::new();
        assert!(!check_admin_auth(&admin, &headers));
    }

    #[tokio::test]
    async fn test_check_admin_auth_empty_headers_insecure_mode() {
        let admin = AdminConfig {
            token: None,
            insecure_no_auth: true,
            outbound_origin_label: None,
        };
        let headers = HeaderMap::new();
        assert!(check_admin_auth(&admin, &headers));
    }

    #[test]
    fn test_is_valid_vtoken_accepts_well_formed() {
        // Real UUID v4 simple form, 32 lowercase hex chars.
        assert!(is_valid_vtoken("vhub_0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn test_is_valid_vtoken_rejects_ilink_style() {
        // SEC-003 hardening: iLink-style bot tokens must never reach the
        // vtoken lookup path.
        assert!(!is_valid_vtoken("botid@im.bot:secret"));
        assert!(!is_valid_vtoken(""));
    }

    #[test]
    fn test_is_valid_vtoken_rejects_wrong_length_and_case() {
        assert!(!is_valid_vtoken("vhub_short"));
        assert!(!is_valid_vtoken("vhub_0123456789ABCDEF0123456789ABCDEF")); // uppercase
        assert!(!is_valid_vtoken("vhub_0123456789abcdef0123456789abcde")); // 31 hex
        assert!(!is_valid_vtoken("vhub_0123456789abcdef0123456789abcdef0")); // 33 hex
    }

    #[test]
    fn test_is_valid_vtoken_rejects_non_hex_suffix() {
        assert!(!is_valid_vtoken("vhub_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
    }

    #[test]
    fn test_extract_vtoken_filters_invalid_format() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer botid@im.bot:secret".parse().unwrap(),
        );
        assert!(extract_vtoken(&headers).is_none());
    }
}
