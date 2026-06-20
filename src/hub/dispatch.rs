//! Inbound message dispatching: the broadcast→backend pipeline, quote-reply
//! resolution, `@mention` routing, and the per-conversation `HubExt` helpers.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};
use crate::store::Store;

// Hub-internal items (HubState, Metrics, MessageQueue, RoutingDecision,
// QuoteOrigin, merge_routing_with_quote, the `router`/`quote_route`/`messages`
// modules, …) are re-exported by the `hub` module.
use super::*;

// ─── Quote index background eviction ─────────────────────────────────────────

pub fn spawn_quote_index_evictor(state: Arc<HubState>) {
    let mut shutdown = state.ilink.shutdown.clone();
    tokio::spawn(async move {
        const EVICT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tokio::time::sleep(EVICT_INTERVAL) => {
                    state.routing.quote_index.lock().await.evict_expired();
                }
            }
        }
    });
}

// ─── Message Dispatcher ───────────────────────────────────────────────────────

pub fn spawn_dispatcher(state: Arc<HubState>, mut rx: broadcast::Receiver<WeixinMessage>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    dispatch_message(state.clone(), msg).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "dispatcher lagged behind upstream");
                    state
                        .metrics
                        .dispatcher_lagged
                        .fetch_add(n, Ordering::Relaxed);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("upstream broadcast channel closed, dispatcher exiting");
                    return;
                }
            }
        }
    });
}

#[tracing::instrument(
    skip_all,
    fields(
        from = msg.from_user_id.as_deref().unwrap_or("?"),
        ctx  = msg.context_token.as_deref().unwrap_or("(none)"),
        msg_type = msg.message_type.unwrap_or(0),
    )
)]
async fn dispatch_message(state: Arc<HubState>, mut msg: WeixinMessage) {
    // RAII guard records dispatch latency on every return path — including
    // early returns for bot copies, @-mention shortcuts, and missing context_token.
    // The `tokio::spawn` persist task below is excluded because the guard drops
    // when *this* async frame returns, before the spawned task runs.
    // We clone the Arc<Metrics> so the guard owns an independent reference and
    // doesn't borrow `state` — `state` is moved into `handle_at_mention` on the
    // @-mention early-return path, which would otherwise conflict with the borrow.
    struct DispatchLatencyGuard {
        start: std::time::Instant,
        metrics: Arc<crate::hub::Metrics>,
    }
    impl Drop for DispatchLatencyGuard {
        fn drop(&mut self) {
            self.metrics
                .dispatch_latency_ms
                .observe(self.start.elapsed().as_millis() as u64);
        }
    }
    let _latency_guard = DispatchLatencyGuard {
        start: std::time::Instant::now(),
        metrics: Arc::clone(&state.metrics),
    };

    // iLink does not echo bot-authored messages back through getupdates in practice, but
    // guard regardless: a bot copy (message_type == 2) must never be routed as a user message
    // (that would forward the Hub's own reply back into the backends).
    if msg.message_type == Some(2) {
        return;
    }

    state
        .metrics
        .upstream_user_messages
        .fetch_add(1, Ordering::Relaxed);

    // `@<backend> <message>` shortcut — highest priority, ahead of quote-reply and the
    // current `/use` route. It is a *temporary* operation (like a quote): it forwards this
    // one message to the named backend on a **fresh session**, without changing the user's
    // active backend or active session. An unknown name falls through to normal routing.
    if let Some(text) = msg.text() {
        if let Some((backend_name, payload)) = router::parse_at_mention(text) {
            let vtoken = {
                let registry = state.clients.registry.read().await;
                registry
                    .get_by_name(&backend_name)
                    .map(|c| c.vtoken.clone())
            };
            if let Some(vtoken) = vtoken {
                handle_at_mention(state, msg, backend_name, vtoken, payload).await;
                return;
            }
        }
    }

    let routing = {
        let router = state.routing.router.lock().await;
        router.route(&msg)
    };

    let quoted = {
        let scope = msg.from_user_id.as_deref().unwrap_or_default();
        // Only the in-memory index lookup needs the `quote_index` lock. Release it
        // BEFORE the DB/footer fallbacks: those await on the store and registry, and
        // holding `quote_index` across them would serialise every quote-routed message
        // and block the outbound index (sendmessage) and the periodic evictor.
        let from_index = {
            let mut q = state.routing.quote_index.lock().await;
            q.resolve_user_quote(scope, &msg)
        };
        if from_index.is_some() {
            from_index
        } else {
            // Cold index (e.g. after a Hub restart): first try DB lookup, then fall back to
            // footer text parsing as last resort. Neither helper touches `quote_index`.
            let from_db = resolve_quote_from_db(&state, &msg).await;
            if from_db.is_some() {
                from_db
            } else {
                resolve_quote_from_footer(&state, &msg).await
            }
        }
    };
    let routing = merge_routing_with_quote(routing, quoted);

    match routing {
        RoutingDecision::HubInternal(cmd) => {
            super::commands::handle_hub_command(Arc::clone(&state), msg, cmd).await;
        }
        RoutingDecision::ForwardTo {
            vtoken,
            session_override,
        } => {
            let real_ctx = match msg.context_token.clone() {
                Some(ctx) if !ctx.is_empty() => ctx,
                _ => {
                    warn!("message has no context_token, skipping dispatch");
                    return;
                }
            };
            let peer_user_id = msg.from_user_id.clone().unwrap_or_default();
            let group_id = msg.group_id.clone();

            let vctx = resolve_vctx_for_message(
                &state,
                &real_ctx,
                &peer_user_id,
                group_id.as_deref(),
                None,
            )
            .await;

            let hub_ext =
                build_hub_ext_for_vctx(&state.store, &vctx, &vtoken, session_override).await;

            // Fire-and-forget: record user message to history.
            if let Some(content) = msg.text().map(str::to_string) {
                let session_name = hub_ext
                    .as_ref()
                    .and_then(|e| e.session_name.as_deref())
                    .unwrap_or("default")
                    .to_string();
                let store = state.store.clone();
                let (vctx3, vtoken3, peer3) = (vctx.clone(), vtoken.clone(), peer_user_id.clone());
                let sem = state.persist_sem.clone();
                tokio::spawn(async move {
                    let Ok(_permit) = sem.try_acquire() else {
                        return;
                    };
                    if let Err(e) = store
                        .save_message(
                            &vctx3,
                            Some(&vtoken3),
                            &session_name,
                            &peer3,
                            "user",
                            &content,
                        )
                        .await
                    {
                        warn!(error = %e, "failed to save user message to history");
                    }
                });
            }

            msg.context_token = Some(vctx);
            msg.ilink_hub_ext = hub_ext;
            push_to_queue(&state.clients.queue, &state.metrics, &vtoken, msg).await;
        }
        RoutingDecision::Broadcast => {
            let from_user_id = msg.from_user_id.as_deref().unwrap_or("?").to_string();
            let online = {
                let registry = state.clients.registry.read().await;
                registry
                    .online_clients()
                    .iter()
                    .map(|c| c.vtoken.clone())
                    .collect::<Vec<_>>()
            };

            if online.is_empty() {
                warn!(from_user_id, "no online clients to dispatch to");
                state
                    .metrics
                    .messages_dropped
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(real_ctx) = msg.context_token.clone().filter(|c| !c.is_empty()) {
                    let to_uid = msg.from_user_id.as_deref().unwrap_or("");
                    let reply_text = build_no_backend_reply(msg.text());
                    debug!(to = %to_uid, "sending no-backend fallback reply");
                    let reply = SendMessageRequest::reply(real_ctx, reply_text, to_uid);
                    match state.ilink.upstream.send_message(reply).await {
                        Err(e) => error!(error = %e, "failed to send no-clients reply"),
                        Ok(resp) if resp.ret.map(|r| r != 0).unwrap_or(false) => {
                            warn!(ret = resp.ret, errmsg = ?resp.errmsg, "iLink rejected no-clients reply");
                        }
                        Ok(_) => {}
                    }
                } else {
                    warn!(
                        from_user_id,
                        "no context_token in message, cannot send no-clients reply"
                    );
                }
                return;
            }

            let real_ctx = match msg.context_token.clone() {
                Some(ctx) if !ctx.is_empty() => ctx,
                _ => {
                    warn!("broadcast message has no context_token, skipping");
                    return;
                }
            };
            let peer_user_id = msg.from_user_id.clone().unwrap_or_default();
            let group_id = msg.group_id.clone();

            // Shared vctx per conversation: sessions are isolated by (vctx, vtoken) so
            // each backend stays independent, while routing-mode changes (broadcast → /use)
            // don't break session continuity.
            let vctx = resolve_vctx_for_message(
                &state,
                &real_ctx,
                &peer_user_id,
                group_id.as_deref(),
                None,
            )
            .await;
            let vctx_by_vtoken: Vec<(String, String)> =
                online.iter().map(|vt| (vt.clone(), vctx.clone())).collect();

            // Batch-fetch HubExt session data — 2 queries total instead of 2×N.
            let pairs: Vec<(String, String)> = vctx_by_vtoken
                .iter()
                .map(|(vt, vc)| (vc.clone(), vt.clone()))
                .collect();
            let hub_ext_data = match state.store.get_hub_ext_batch(&pairs).await {
                Ok(data) => data,
                Err(e) => {
                    warn!(error = %e, "get_hub_ext_batch failed; broadcast will proceed without HubExt");
                    Default::default()
                }
            };

            for (vtoken, vctx) in vctx_by_vtoken {
                let hub_ext = hub_ext_data.get(&(vctx.clone(), vtoken.clone())).map(
                    |(session_name, session_id)| HubExt {
                        session_id: session_id.clone(),
                        session_name: Some(session_name.clone()),
                        cli_session_id: None,
                    },
                );
                let mut msg_clone = msg.clone();
                msg_clone.context_token = Some(vctx);
                msg_clone.ilink_hub_ext = hub_ext;
                push_to_queue(&state.clients.queue, &state.metrics, &vtoken, msg_clone).await;
            }
        }
    }
}

/// DB-backed quote resolver: query the messages table by peer_user_id + content prefix.
/// Runs when the in-memory QuoteRouteIndex is cold. Returns the vtoken + session_name
/// recorded when the assistant message was originally sent, independent of footer format.
async fn resolve_quote_from_db(
    state: &Arc<HubState>,
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<QuoteOrigin> {
    let (quoted_text, _) = quote_route::QuoteRouteIndex::collect_quoted(msg)?;
    let peer_user_id = msg.from_user_id.as_deref().unwrap_or_default();
    if peer_user_id.is_empty() {
        return None;
    }
    // Use the first 48 chars as prefix (same constant as CONTENT_PREFIX_CHARS in quote_route).
    let prefix: String = quoted_text.trim().chars().take(48).collect();
    if prefix.is_empty() {
        return None;
    }
    match state
        .store
        .find_assistant_message_by_content(peer_user_id, &prefix)
        .await
    {
        Ok(Some((vtoken, session_name))) if !vtoken.is_empty() => {
            let (name, label) = {
                let registry = state.clients.registry.read().await;
                registry
                    .get_by_vtoken(&vtoken)
                    .map(|c| (c.name.clone(), c.label.clone()))
                    .unwrap_or_else(|| (vtoken.clone(), None))
            };
            debug!(
                peer = %peer_user_id,
                vtoken = %crate::redact_token(&vtoken),
                session = ?session_name,
                "quote index miss — resolved via DB message history"
            );
            Some(QuoteOrigin::Client {
                vtoken,
                name,
                label,
                session_name,
            })
        }
        Ok(_) => None,
        Err(e) => {
            warn!(error = %e, "DB quote lookup failed, falling back to footer");
            None
        }
    }
}

/// Fallback quote resolver: parse the origin footer from the quoted message text and look up
/// the backend by name in the client registry. Used when the in-memory quote index is cold
/// (e.g. after a Hub restart).
async fn resolve_quote_from_footer(
    state: &Arc<HubState>,
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<QuoteOrigin> {
    let (name, session_name) = quote_route::QuoteRouteIndex::footer_from_user_quote(msg)?;
    let registry = state.clients.registry.read().await;
    let client = registry.get_by_name(&name)?;
    debug!(
        backend = %name,
        session = ?session_name,
        "quote index miss — resolved via footer fallback"
    );
    Some(QuoteOrigin::Client {
        vtoken: client.vtoken.clone(),
        name: client.name.clone(),
        label: client.label.clone(),
        session_name,
    })
}

/// Handle an `@<backend> <message>` shortcut: forward `payload` to `vtoken` on a brand-new,
/// uniquely-named session, without touching the user's active backend (`/use`) or active
/// session. Each `@` creates a fresh session (product decision); to continue the conversation
/// the user quote-replies to the backend's answer, which the quote index routes back to this
/// session (the echoed `session_name` is registered on the outbound reply).
async fn handle_at_mention(
    state: Arc<HubState>,
    mut msg: WeixinMessage,
    backend_name: String,
    vtoken: String,
    payload: String,
) {
    let real_ctx = match msg.context_token.clone() {
        Some(ctx) if !ctx.is_empty() => ctx,
        _ => {
            warn!("@mention message has no context_token, skipping dispatch");
            return;
        }
    };
    let peer_user_id = msg.from_user_id.clone().unwrap_or_default();
    let group_id = msg.group_id.clone();

    let vctx =
        resolve_vctx_for_message(&state, &real_ctx, &peer_user_id, group_id.as_deref(), None).await;

    // Always a new session. Millisecond precision keeps names unique even for rapid @-mentions.
    let session_name = format!("at-{}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f"));

    // Pre-create the (empty-UUID) session slot so it shows up in `/session list` immediately and
    // is a real, resumable session once the backend replies with its cli_session_id. We do NOT
    // mark it active — that would change the user's current session, defeating the "temporary"
    // semantics.
    if let Err(e) = state
        .store
        .set_backend_session(&vctx, &vtoken, &session_name, "")
        .await
    {
        warn!(error = %e, vctx = %vctx, session = %session_name, "failed to pre-create @mention session slot");
    }

    debug!(
        backend = %backend_name,
        vtoken = %crate::redact_token(&vtoken),
        session = %session_name,
        "routing @mention to new session"
    );

    let hub_ext =
        build_hub_ext_for_vctx(&state.store, &vctx, &vtoken, Some(session_name.clone())).await;

    // Strip the `@name` prefix so the backend receives only the message body.
    set_first_text_item(&mut msg, payload);
    msg.context_token = Some(vctx);
    msg.ilink_hub_ext = hub_ext;
    push_to_queue(&state.clients.queue, &state.metrics, &vtoken, msg).await;
}

/// Replace the text of the first text-bearing item in `msg` (used to strip the `@name` prefix
/// before forwarding). If no text item exists, the message is left unchanged.
fn set_first_text_item(msg: &mut WeixinMessage, text: String) {
    let Some(items) = msg.item_list.as_mut() else {
        return;
    };
    let items_mut = std::sync::Arc::make_mut(items);
    if let Some(item) = items_mut.iter_mut().find(|i| i.text_item.is_some()) {
        if let Some(ti) = item.text_item.as_mut() {
            ti.text = Some(text);
        }
    }
}

/// Push a prepared message to the per-client queue and update metrics.
async fn push_to_queue(
    queue: &Arc<dyn MessageQueue>,
    metrics: &Metrics,
    vtoken: &str,
    msg: WeixinMessage,
) {
    match queue.push(vtoken, msg).await {
        Ok(false) => {
            metrics.messages_dispatched.fetch_add(1, Ordering::Relaxed);
        }
        Ok(true) => {
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(error = %e, vtoken = %crate::redact_token(vtoken), "failed to push message to queue");
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ─── Hub extension helper ─────────────────────────────────────────────────────

/// Resolve (or create) a stable virtual context token for this conversation.
///
/// WeChat/iLink may send a new `real_ctx` on every inbound message even in the same DM.
/// Reusing one vctx per peer/group keeps backend session IDs (Claude `--resume`, etc.)
/// attached across turns.
///
/// All lookups go directly to the DB via `store.find_or_create_vctx`.
pub(super) async fn resolve_vctx_for_message(
    state: &HubState,
    real_ctx: &str,
    peer_user_id: &str,
    group_id: Option<&str>,
    _client_scope: Option<&str>,
) -> String {
    match state
        .store
        .find_or_create_vctx(peer_user_id, group_id, real_ctx)
        .await
    {
        Ok(vctx) => vctx,
        Err(e) => {
            warn!(error = %e, "find_or_create_vctx failed, generating ephemeral vctx");
            format!("vctx_{}", uuid::Uuid::new_v4().simple())
        }
    }
}

/// Build `HubExt` for an outbound message.
///
/// Sessions are scoped to `(vctx, vtoken)` so that each backend has its own independent
/// session namespace for the same WeChat conversation.
///
/// When `session_override` is provided (from a quote-reply), that session is used directly
/// instead of the current active session, so the message is routed to the correct conversation.
pub(super) async fn build_hub_ext_for_vctx(
    store: &Store,
    vctx: &str,
    vtoken: &str,
    session_override: Option<String>,
) -> Option<HubExt> {
    let session_name = match session_override {
        Some(name) if !name.is_empty() => name,
        _ => match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.get_active_session_name(vctx, vtoken),
        )
        .await
        {
            Ok(Ok(name)) => name,
            Ok(Err(e)) => {
                warn!("Failed to get active session name from DB for {vctx}: {e}");
                "default".to_string()
            }
            Err(_) => {
                warn!("Timeout getting active session name from DB for {vctx}");
                "default".to_string()
            }
        },
    };

    let session_id = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.get_backend_session(vctx, vtoken, &session_name),
    )
    .await
    {
        Ok(Ok(Some(s))) => {
            let t = s.trim().to_string();
            (!t.is_empty()).then_some(t)
        }
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            warn!("Failed to get backend session from DB for {vctx}/{session_name}: {e}");
            None
        }
        Err(_) => {
            warn!("Timeout getting backend session from DB for {vctx}/{session_name}");
            None
        }
    };

    Some(HubExt {
        session_id,
        session_name: Some(session_name),
        cli_session_id: None,
    })
}

/// Reply text when no AI backend is online.
/// Varies slightly based on whether the user sent a hub command (handled separately)
/// or a regular message.
fn build_no_backend_reply(user_text: Option<&str>) -> String {
    let is_command = user_text
        .map(|t| t.trim().starts_with('/'))
        .unwrap_or(false);

    if is_command {
        // User tried a command — give a hint that /help is available
        return messages::UNRECOGNIZED_COMMAND.to_string();
    }

    "你好！我是 iLink Hub 消息路由服务。\n\
     \n\
     当前暂无 AI 助手后端在线，您的消息暂时无法被处理。\n\
     \n\
     您可以：\n\
     • 发送 /status 查看服务状态\n\
     • 发送 /list   查看已注册的后端\n\
     • 发送 /help   查看完整帮助\n\
     \n\
     如需接入 AI 助手，请联系管理员配置后端服务。"
        .to_string()
}
