//! iLink-compatible bot API routes (register + getupdates/sendmessage/…).
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, error, warn};

use super::auth::{check_admin_auth, extract_vtoken, UNKNOWN_VTOKEN_MSG};
use super::wait::wait_notify_or_shutdown;
use crate::hub::{HubState, MAX_CONCURRENT_POLLS_PER_VTOKEN};
use crate::ilink::types::*;
use crate::redact_token;
use crate::server::pairing::register_client_in_hub;

type HistoGuard<'a> = crate::hub::LatencyGuard<'a>;

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
