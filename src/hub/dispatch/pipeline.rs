//! Broadcast→backend dispatch pipeline entry points.
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};

use super::super::*;
use super::hub_ext::{build_hub_ext_for_vctx, build_no_backend_reply, resolve_vctx_for_message};
use super::mention::handle_at_mention;
use super::queue::{push_shared_to_queue, push_to_queue};
use super::quote::{
    resolve_quote_from_db, resolve_quote_from_footer, resolve_quote_from_msg_id,
    resolve_quote_from_timestamp,
};

pub fn spawn_dispatcher(state: Arc<HubState>, mut rx: mpsc::Receiver<WeixinMessage>) {
    tokio::spawn(async move {
        let mut shutdown = state.ilink.shutdown.clone();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("dispatcher shutting down");
                        return;
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            dispatch_message(state.clone(), msg).await;
                        }
                        None => {
                            info!("upstream channel closed, dispatcher exiting");
                            return;
                        }
                    }
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
pub(super) async fn dispatch_message(state: Arc<HubState>, mut msg: WeixinMessage) {
    // RAII guard: records dispatch latency on every return path.
    // Clones metrics Arc so the guard doesn't borrow `state` (state is moved
    // into handle_at_mention on the @-mention path, which conflicts with a borrow).
    let metrics_arc = Arc::clone(&state.metrics);
    let _latency_guard = crate::hub::LatencyGuard::new(&metrics_arc.dispatch_latency_ms);

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
                    .get_by_alias(&backend_name)
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

    // Pre-extract ref_ms to detect "has quote but all fallbacks missed" later without
    // re-running the item scan after the resolution block.
    let ref_ms_hint = quote_route::collect_quoted_timestamp(&msg);
    if let Some(ref_id) = quote_route::collect_quoted_msg_id(&msg) {
        debug!(
            quoted_msg_id = %ref_id,
            ref_ms = ?ref_ms_hint,
            peer = ?msg.from_user_id.as_deref().unwrap_or("?"),
            "inbound quote-reply ref_msg"
        );
    }
    let quoted = {
        // L0: exact match on the iLink-preserved `ref_msg.message_item.msg_id`
        // (unique id of the quoted assistant message). Tried first because it is
        // unambiguous; falls through to the timestamp/content/footer fallbacks
        // for pre-feature rows or user-side quotes that carry no Hub msg_id.
        let from_msg_id = resolve_quote_from_msg_id(&state, &msg).await;
        if from_msg_id.is_some() {
            from_msg_id
        } else {
            // L1: timestamp lookup (iLink always provides create_time_ms even
            // when text is absent), then L2 content-prefix DB lookup, then L3
            // footer text parsing as last resort.
            let from_ts = resolve_quote_from_timestamp(&state, &msg).await;
            if from_ts.is_some() {
                from_ts
            } else {
                let from_db = resolve_quote_from_db(&state, &msg).await;
                if from_db.is_some() {
                    from_db
                } else {
                    resolve_quote_from_footer(&state, &msg).await
                }
            }
        }
    };
    if quoted.is_none() {
        if let Some(ref_ms) = ref_ms_hint {
            state
                .metrics
                .quote_resolve_miss_total
                .fetch_add(1, Ordering::Relaxed);
            debug!(ref_ms, "all quote fallbacks missed, using base routing");
        }
    }
    let routing = merge_routing_with_quote(routing, quoted);

    match routing {
        RoutingDecision::HubInternal(cmd) => {
            crate::hub::commands::handle_hub_command(Arc::clone(&state), msg, cmd).await;
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

            // Grant (vctx, vtoken) ownership BEFORE pushing to the queue so the
            // bridge's first sendmessage cannot race ahead of the grant row.
            // Also resets a2a_depth to 0 for subsequent `call_agent` calls.
            let session_name_for_depth = hub_ext
                .as_ref()
                .and_then(|e| e.session_name.as_deref())
                .unwrap_or("default")
                .to_string();
            if let Err(e) = state
                .store
                .set_active_session_with_depth(&vctx, &vtoken, &session_name_for_depth, 0)
                .await
            {
                warn!(error = %e, "failed to grant vctx ownership / reset a2a_depth on dispatch");
            }

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
            handle_broadcast(state, msg).await;
        }
    }
}

/// Fan-out path for [`RoutingDecision::Broadcast`]: reply when no backends are
/// online, otherwise push a shared copy to every online client.
///
/// Extracted so unit tests can exercise the arm directly — `Router::route`
/// currently never returns `Broadcast` (falls through to Help instead), but
/// the arm remains the intended fan-out implementation if routing re-enables it.
pub(super) async fn handle_broadcast(state: Arc<HubState>, msg: WeixinMessage) {
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
    let vctx =
        resolve_vctx_for_message(&state, &real_ctx, &peer_user_id, group_id.as_deref(), None).await;
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

    // Share the unchanged fields of the message across recipients —
    // only `context_token` and `ilink_hub_ext` differ per vtoken, so
    // we pass the base through `Arc` and let the queue clone it once
    // per recipient instead of doing N full `WeixinMessage::clone`s
    // on the broadcast path.
    let shared_msg: Arc<WeixinMessage> = Arc::new(msg);
    for (vtoken, vctx) in vctx_by_vtoken {
        let hub_ext =
            hub_ext_data
                .get(&(vctx.clone(), vtoken.clone()))
                .map(|(session_name, session_id)| HubExt {
                    session_id: session_id.clone(),
                    session_name: Some(session_name.clone()),
                    cli_session_id: None,
                    a2a_call_id: None,
                    a2a_depth: None,
                    usage: None,
                });
        // Grant ownership before the bridge can reply (same as ForwardTo path).
        let session_name = hub_ext
            .as_ref()
            .and_then(|e| e.session_name.as_deref())
            .unwrap_or("default");
        if let Err(e) = state
            .store
            .set_active_session_with_depth(&vctx, &vtoken, session_name, 0)
            .await
        {
            warn!(error = %e, "failed to grant vctx ownership on broadcast dispatch");
        }
        push_shared_to_queue(
            &state.clients.queue,
            &state.metrics,
            &vtoken,
            Arc::clone(&shared_msg),
            Some(vctx),
            hub_ext,
        )
        .await;
    }
}
