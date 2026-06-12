pub mod health;
pub mod messages;
pub mod outbound_label;
pub mod pairing;
pub mod queue;
pub mod quote_route;
pub mod registry;
pub mod router;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};
use crate::ilink::{QrLoginUiEvent, UpstreamClient};
use crate::store::Store;

/// iLink upstream connection status codes stored in `HubState::ilink_status`.
pub mod ilink_status {
    pub const UNKNOWN: u8 = 0;
    pub const CONNECTED: u8 = 1;
    pub const NEEDS_LOGIN: u8 = 2;
    pub const LOGGING_IN: u8 = 3;
}

pub use health::spawn_health_checker;
pub use outbound_label::{
    append_outbound_origin_footer_to_first_text_item, format_outbound_origin_line,
    should_append_outbound_origin_label,
};
pub use pairing::PairingRegistry;
pub use queue::{conversation_key, ClientQueue, ContextTokenMap, InMemoryQueue, MessageQueue};
pub use quote_route::{merge_routing_with_quote, QuoteRouteIndex};
pub use registry::{ClientInfo, ClientRegistry};
pub use router::{HubCommand, Router, RoutingDecision};

pub use ilink_status as IlinkStatus;

// ─── Metrics ──────────────────────────────────────────────────────────────────

pub struct Metrics {
    pub messages_dispatched: AtomicU64,
    pub messages_dropped: AtomicU64,
    /// User-side (or command) messages taken from upstream and passed into routing
    /// (excludes bot-side echo copies with `message_type == 2`).
    pub upstream_user_messages: AtomicU64,
    /// Total sendmessage calls from backend clients.
    pub sendmessage_total: AtomicU64,
    /// sendmessage calls that were rejected (unknown token, missing context, etc.).
    pub sendmessage_errors: AtomicU64,
    /// Number of QR re-login attempts triggered (manual or automatic).
    pub relogin_attempts: AtomicU64,
    /// Number of messages missed because the dispatcher lagged behind the broadcast channel.
    pub dispatcher_lagged: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_dispatched: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            upstream_user_messages: AtomicU64::new(0),
            sendmessage_total: AtomicU64::new(0),
            sendmessage_errors: AtomicU64::new(0),
            relogin_attempts: AtomicU64::new(0),
            dispatcher_lagged: AtomicU64::new(0),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Concurrent long-poll tracker ─────────────────────────────────────────────

/// Tracks how many `getupdates` long-polls are concurrently active per vtoken.
///
/// A healthy backend has at most one process polling its vtoken at a time. Two or more
/// concurrent polls for the same vtoken mean multiple bridge processes share one
/// credential/token and are competing for the same per-vtoken message queue (`drain` is a
/// destructive read), so inbound messages get stolen non-deterministically. This tracker
/// lets the Hub surface that misconfiguration instead of failing silently.
#[derive(Default)]
pub struct PollTracker {
    counts: StdMutex<HashMap<String, usize>>,
}

impl PollTracker {
    /// Register a new active poll for `vtoken`. Returns the number of polls now concurrently
    /// active for that vtoken (always >= 1) and a guard that decrements the count on drop.
    pub fn enter(self: &Arc<Self>, vtoken: &str) -> (usize, PollGuard) {
        let count = {
            let mut counts = self.counts.lock().unwrap();
            let c = counts.entry(vtoken.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        (
            count,
            PollGuard {
                tracker: Arc::clone(self),
                vtoken: vtoken.to_string(),
            },
        )
    }
}

/// RAII guard returned by [`PollTracker::enter`]; decrements the per-vtoken poll count when
/// the long-poll handler returns (success, timeout, shutdown, or client disconnect).
pub struct PollGuard {
    tracker: Arc<PollTracker>,
    vtoken: String,
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        let mut counts = self.tracker.counts.lock().unwrap();
        if let Some(c) = counts.get_mut(&self.vtoken) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                counts.remove(&self.vtoken);
            }
        }
    }
}

// ─── Shared Hub State ─────────────────────────────────────────────────────────

pub struct HubState {
    pub upstream: Arc<UpstreamClient>,
    pub registry: RwLock<ClientRegistry>,
    pub pairing: RwLock<PairingRegistry>,
    pub queue: Arc<dyn MessageQueue>,
    pub ctx_map: RwLock<ContextTokenMap>,
    pub router: Mutex<Router>,
    /// Quote-reply → backend / hub command (see [`quote_route`]).
    pub quote_index: Mutex<QuoteRouteIndex>,
    pub store: Arc<Store>,
    pub metrics: Metrics,
    /// Shared with Axum graceful shutdown; long-poll handlers exit early when this becomes `true`.
    pub shutdown: watch::Receiver<bool>,
    /// Current iLink upstream status (see [`ilink_status`] constants).
    pub ilink_status: Arc<AtomicU8>,
    /// Broadcasts QR login UI events to SSE subscribers.
    pub qr_tx: broadcast::Sender<QrLoginUiEvent>,
    /// Last QR Ready event — replayed to new SSE subscribers that connect after it was sent.
    pub qr_last_ready: Arc<Mutex<Option<QrLoginUiEvent>>>,
    /// Signals the polling loop to initiate a fresh QR re-login.
    pub relogin_tx: broadcast::Sender<()>,
    /// Tracks concurrent `getupdates` long-polls per vtoken to detect bridges that share one
    /// credential/token (queue split-brain).
    pub poll_tracker: Arc<PollTracker>,
}

impl HubState {
    pub fn new(
        upstream: Arc<UpstreamClient>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        let (qr_tx, _) = broadcast::channel(16);
        let (relogin_tx, _) = broadcast::channel(4);
        Arc::new(Self {
            upstream,
            registry: RwLock::new(ClientRegistry::new()),
            pairing: RwLock::new(PairingRegistry::new()),
            queue,
            ctx_map: RwLock::new(ContextTokenMap::default()),
            router: Mutex::new(Router::new(None)),
            quote_index: Mutex::new(QuoteRouteIndex::default()),
            store,
            metrics: Metrics::new(),
            shutdown,
            ilink_status: Arc::new(AtomicU8::new(ilink_status::UNKNOWN)),
            qr_tx,
            qr_last_ready: Arc::new(Mutex::new(None)),
            relogin_tx,
            poll_tracker: Arc::new(PollTracker::default()),
        })
    }
}

// ─── Quote index background eviction ─────────────────────────────────────────

pub fn spawn_quote_index_evictor(state: Arc<HubState>) {
    let mut shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        const EVICT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tokio::time::sleep(EVICT_INTERVAL) => {
                    state.quote_index.lock().await.evict_expired();
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

    let routing = {
        let router = state.router.lock().await;
        router.route(&msg)
    };

    let quoted = {
        let scope = msg.from_user_id.as_deref().unwrap_or_default();
        let mut q = state.quote_index.lock().await;
        q.resolve_user_quote(scope, &msg)
    };
    let routing = merge_routing_with_quote(routing, quoted);

    match routing {
        RoutingDecision::HubInternal(cmd) => {
            handle_hub_command(state, msg, cmd).await;
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

            let store = state.store.clone();
            let (vctx2, real2, peer2) = (vctx.clone(), real_ctx.clone(), peer_user_id.clone());
            tokio::spawn(async move {
                if let Err(e) = store.persist_context_token(&vctx2, &real2, &peer2).await {
                    warn!(error = %e, "failed to persist context_token mapping");
                }
            });

            let hub_ext =
                build_hub_ext_for_vctx(&state.store, &vctx, &vtoken, session_override).await;
            msg.context_token = Some(vctx);
            msg.ilink_hub_ext = hub_ext;
            push_to_queue(&state, &vtoken, msg).await;
        }
        RoutingDecision::Broadcast => {
            let from_user_id = msg.from_user_id.as_deref().unwrap_or("?").to_string();
            let online = {
                let registry = state.registry.read().await;
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
                    match state.upstream.send_message(reply).await {
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

            // Batch-persist in one transaction (fire-and-forget).
            {
                let entries: Vec<(String, String, String)> = vctx_by_vtoken
                    .iter()
                    .map(|(_, vc)| (vc.clone(), real_ctx.clone(), peer_user_id.clone()))
                    .collect();
                let store = state.store.clone();
                tokio::spawn(async move {
                    if let Err(e) = store.persist_context_tokens_batch(&entries).await {
                        warn!(error = %e, "failed to batch-persist context_token mappings (broadcast)");
                    }
                });
            }

            // Batch-fetch HubExt session data — 2 queries total instead of 2×N.
            let pairs: Vec<(String, String)> = vctx_by_vtoken
                .iter()
                .map(|(vt, vc)| (vc.clone(), vt.clone()))
                .collect();
            let hub_ext_data = state
                .store
                .get_hub_ext_batch(&pairs)
                .await
                .unwrap_or_default();

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
                push_to_queue(&state, &vtoken, msg_clone).await;
            }
        }
    }
}

/// Push a prepared message to the per-client queue and update metrics.
async fn push_to_queue(state: &HubState, vtoken: &str, msg: WeixinMessage) {
    match state.queue.push(vtoken, msg).await {
        Ok(false) => {
            state
                .metrics
                .messages_dispatched
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(true) => {
            state
                .metrics
                .messages_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(error = %e, vtoken = %&vtoken[..vtoken.len().min(8)], "failed to push message to queue");
            state
                .metrics
                .messages_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Resolve the vctx and currently routed vtoken for a Hub command from a given user.
/// Returns `None` if no backend is selected (broadcasts a NO_BACKEND message via the caller).
async fn resolve_vctx_and_vtoken(
    state: &HubState,
    real_ctx: &str,
    from_user_id: &str,
    group_id: Option<&str>,
) -> (String, Option<String>) {
    let vctx = resolve_vctx_for_message(state, real_ctx, from_user_id, group_id, None).await;
    let vtoken = state
        .router
        .lock()
        .await
        .get_route(from_user_id)
        .map(str::to_string);
    (vctx, vtoken)
}

async fn handle_hub_command(state: Arc<HubState>, msg: WeixinMessage, cmd: HubCommand) {
    let real_ctx = match msg.context_token.clone() {
        Some(ctx) if !ctx.is_empty() => ctx,
        _ => {
            warn!(
                ?cmd,
                "hub command message has no context_token, cannot reply"
            );
            return;
        }
    };
    let from_user_id = msg.from_user_id.as_deref().unwrap_or_default().to_string();
    debug!(?cmd, from_user_id, context_token = %real_ctx, "handling hub command");

    let reply_text = match cmd {
        HubCommand::List => {
            let registry = state.registry.read().await;
            let clients = registry.all_clients();
            if clients.is_empty() {
                "尚未注册任何后端客户端。".to_string()
            } else {
                let active_vtoken = {
                    let router = state.router.lock().await;
                    router.get_route(&from_user_id).map(str::to_string)
                };
                let active_name = active_vtoken.as_deref().and_then(|vt| {
                    clients
                        .iter()
                        .find(|c| c.vtoken == vt)
                        .map(|c| c.name.as_str())
                });
                let mut lines = vec!["**已注册的后端：**".to_string()];
                for c in clients {
                    let status = if c.online { "🟢" } else { "🔴" };
                    let label = c.label.as_deref().unwrap_or(&c.name);
                    let selected = if active_name == Some(c.name.as_str()) {
                        " ✅"
                    } else {
                        ""
                    };
                    lines.push(format!("{} `{}`{} — {}", status, c.name, selected, label));
                }
                match active_name {
                    Some(name) => lines.push(format!("\n当前选中：`{}`", name)),
                    None => lines.push("\n当前未选中（广播模式）".to_string()),
                }
                lines.push("用 `/use <名称>` 切换后端。".to_string());
                lines.join("\n")
            }
        }
        HubCommand::UseClient(ref name) => {
            let registry = state.registry.read().await;
            if let Some(client) = registry.get_by_name(name) {
                let vtoken = client.vtoken.clone();
                drop(registry);

                {
                    let mut router = state.router.lock().await;
                    router.set_route(&from_user_id, vtoken.clone());
                }

                if let Err(e) = state.store.set_route(&from_user_id, &vtoken).await {
                    warn!(error = %e, "failed to persist route to DB");
                }

                format!("✅ 已切换到 `{}`", name)
            } else {
                format!("❌ 未找到名为 `{}` 的后端。用 `/list` 查看可用后端。", name)
            }
        }
        HubCommand::Broadcast(ref text) => {
            let online = {
                let registry = state.registry.read().await;
                registry
                    .online_clients()
                    .iter()
                    .map(|c| c.vtoken.clone())
                    .collect::<Vec<_>>()
            };
            for vtoken in &online {
                // Unscoped vctx (see broadcast note in dispatch_message): keeps each backend's
                // session namespace consistent between directed and broadcast routing.
                let vctx = resolve_vctx_for_message(
                    &state,
                    &real_ctx,
                    &from_user_id,
                    msg.group_id.as_deref(),
                    None,
                )
                .await;
                let mut m = msg.clone();
                let hub_ext = build_hub_ext_for_vctx(&state.store, &vctx, vtoken, None).await;
                m.context_token = Some(vctx.clone());
                m.ilink_hub_ext = hub_ext;
                // Replace text content in item_list
                if let Some(items) = &mut m.item_list {
                    if let Some(first) = items.first_mut() {
                        if let Some(ti) = &mut first.text_item {
                            ti.text = Some(text.clone());
                        }
                    }
                }
                match state.queue.push(vtoken, m).await {
                    Ok(false) => {
                        state
                            .metrics
                            .messages_dispatched
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(true) => {
                        state
                            .metrics
                            .messages_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!(error = %e, vtoken = %&vtoken[..vtoken.len().min(8)], "failed to push hub broadcast message");
                        state
                            .metrics
                            .messages_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            format!("📡 Broadcast to {} client(s)", online.len())
        }
        HubCommand::Status => {
            let registry = state.registry.read().await;
            let online = registry.online_clients().len();
            let total = registry.all_clients().len();
            messages::hub_status(online, total)
        }
        HubCommand::Help => build_help_text(),

        HubCommand::SessionList => {
            let (vctx, vtoken) =
                resolve_vctx_and_vtoken(&state, &real_ctx, &from_user_id, msg.group_id.as_deref())
                    .await;
            match vtoken {
                None => messages::NO_BACKEND.to_string(),
                Some(vtoken) => {
                    // Resolve the backend display name for the reply header.
                    let backend_name = {
                        let registry = state.registry.read().await;
                        registry
                            .all_clients()
                            .into_iter()
                            .find(|c| c.vtoken == vtoken)
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| vtoken.clone())
                    };
                    let active = state
                        .store
                        .get_active_session_name(&vctx, &vtoken)
                        .await
                        .unwrap_or_else(|_| "default".to_string());
                    match state.store.list_backend_sessions(&vctx, &vtoken).await {
                        Ok(sessions) if sessions.is_empty() => {
                            format!(
                                "当前后端 `{backend_name}` {}",
                                messages::SESSION_LIST_NO_SESSIONS
                            )
                        }
                        Ok(sessions) => {
                            let mut lines =
                                vec![format!("**后端 `{backend_name}` 的 sessions：**")];
                            for s in &sessions {
                                let marker = if s.session_name == active { " ✅" } else { "" };
                                let uuid_hint = if s.backend_session_id.is_empty() {
                                    messages::SESSION_SLOT_NO_UUID.to_string()
                                } else {
                                    format!(
                                        "`{}`",
                                        &s.backend_session_id[..s.backend_session_id.len().min(12)]
                                    )
                                };
                                lines.push(format!(
                                    "• `{}`{} — {}",
                                    s.session_name, marker, uuid_hint
                                ));
                            }
                            lines.push(format!("\n当前活跃：`{}`", active));
                            lines.push(messages::SESSION_LIST_SWITCH_HINT.to_string());
                            lines.join("\n")
                        }
                        Err(e) => messages::session_list_failed(&e),
                    }
                }
            }
        }

        HubCommand::SessionNew(ref session_name, ref initial_uuid) => {
            let (vctx, vtoken) =
                resolve_vctx_and_vtoken(&state, &real_ctx, &from_user_id, msg.group_id.as_deref())
                    .await;
            match vtoken {
                None => messages::NO_BACKEND.to_string(),
                Some(vtoken) => {
                    match state
                        .store
                        .set_backend_session(&vctx, &vtoken, session_name, initial_uuid)
                        .await
                    {
                        Ok(()) => {
                            let switch_result = state
                                .store
                                .set_active_session_name(&vctx, &vtoken, session_name)
                                .await;
                            match switch_result {
                                Ok(()) => messages::session_new_ok(session_name),
                                Err(e) => {
                                    messages::session_new_created_switch_failed(session_name, &e)
                                }
                            }
                        }
                        Err(e) => messages::session_new_failed(&e),
                    }
                }
            }
        }

        HubCommand::SessionUse(ref session_name) => {
            let (vctx, vtoken) =
                resolve_vctx_and_vtoken(&state, &real_ctx, &from_user_id, msg.group_id.as_deref())
                    .await;
            match vtoken {
                None => messages::NO_BACKEND.to_string(),
                Some(vtoken) => {
                    // Ensure the session exists (auto-create slot with empty UUID if not)
                    let ensure_result: Result<(), String> = match state
                        .store
                        .get_backend_session(&vctx, &vtoken, session_name)
                        .await
                    {
                        Ok(None) => state
                            .store
                            .set_backend_session(&vctx, &vtoken, session_name, "")
                            .await
                            .map_err(|e| messages::session_use_slot_create_failed(&e)),
                        Err(e) => Err(messages::session_use_query_failed(&e)),
                        Ok(Some(_)) => Ok(()),
                    };
                    match ensure_result {
                        Err(msg) => msg,
                        Ok(()) => {
                            match state
                                .store
                                .set_active_session_name(&vctx, &vtoken, session_name)
                                .await
                            {
                                Ok(()) => messages::session_use_ok(session_name),
                                Err(e) => messages::session_use_failed(&e),
                            }
                        }
                    }
                }
            }
        }

        HubCommand::SessionDelete(ref session_name) => {
            let (vctx, vtoken) =
                resolve_vctx_and_vtoken(&state, &real_ctx, &from_user_id, msg.group_id.as_deref())
                    .await;
            match vtoken {
                None => messages::NO_BACKEND.to_string(),
                Some(vtoken) => {
                    let active = state
                        .store
                        .get_active_session_name(&vctx, &vtoken)
                        .await
                        .unwrap_or_else(|_| "default".to_string());
                    if *session_name == active {
                        messages::session_delete_active_error(session_name)
                    } else {
                        match state
                            .store
                            .delete_backend_session(&vctx, &vtoken, session_name)
                            .await
                        {
                            Ok(true) => messages::session_delete_ok(session_name),
                            Ok(false) => messages::session_delete_not_found(session_name),
                            Err(e) => messages::session_delete_failed(&e),
                        }
                    }
                }
            }
        }
    };

    debug!(to = %from_user_id, "sending hub command reply");
    let mut send_req = SendMessageRequest::reply(real_ctx, reply_text, &from_user_id);
    if let Some(m) = &mut send_req.msg {
        m.ensure_outbound();
        let index_hub_quote = matches!(
            &cmd,
            HubCommand::List
                | HubCommand::Status
                | HubCommand::Help
                | HubCommand::UseClient(_)
                | HubCommand::SessionList
                | HubCommand::SessionNew(_, _)
                | HubCommand::SessionUse(_)
                | HubCommand::SessionDelete(_)
        );
        if index_hub_quote {
            // Content path: works even though iLink never echoes the bot copy back.
            if let Some(text) = m.text().map(str::to_string) {
                state.quote_index.lock().await.register_outbound_content(
                    &from_user_id,
                    &text,
                    quote_route::QuoteOrigin::Hub { cmd: cmd.clone() },
                );
            }
        }
    }
    match state.upstream.send_message(send_req).await {
        Err(e) => error!(error = %e, "failed to send hub command reply"),
        Ok(resp) if resp.ret.map(|r| r != 0).unwrap_or(false) => {
            error!(ret = resp.ret, errmsg = ?resp.errmsg, "iLink rejected hub command reply");
        }
        Ok(_) => {
            debug!(?cmd, "hub command reply sent successfully");
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
/// `client_scope` can pin the vctx to a specific backend (`Some(vtoken)`), but all current
/// callers pass `None`: directed (`/use`) and broadcast share one conversation-stable vctx,
/// and per-backend session isolation comes from the `(vctx, vtoken)` session key instead.
/// Keeping the parameter leaves the door open for per-backend context scoping if ever needed.
async fn resolve_vctx_for_message(
    state: &HubState,
    real_ctx: &str,
    peer_user_id: &str,
    group_id: Option<&str>,
    client_scope: Option<&str>,
) -> String {
    let conv_key = conversation_key(peer_user_id, group_id).map(|k| match client_scope {
        Some(scope) => format!("{k}@{scope}"),
        None => k,
    });

    // For unscoped DM routing only: check once under the lock whether we need a DB seed.
    // Do the DB lookup outside the lock (it's async/slow), then re-acquire to seed+map
    // atomically so no other task can race in between.
    let db_vctx = if client_scope.is_none() {
        let needs_seed = if let Some(ref key) = conv_key {
            !state.ctx_map.read().await.has_conversation(key)
        } else {
            false
        };
        if needs_seed
            && conv_key
                .as_deref()
                .map(|k| k.starts_with("peer:"))
                .unwrap_or(false)
        {
            state
                .store
                .find_vctx_for_peer(peer_user_id)
                .await
                .ok()
                .flatten()
        } else {
            None
        }
    } else {
        None
    };

    let mut ctx_map = state.ctx_map.write().await;
    if let (Some(ref key), Some(vctx)) = (&conv_key, db_vctx) {
        // Only seed if nothing else raced in while we held the lock released.
        if !ctx_map.has_conversation(key) {
            ctx_map.seed_conversation(
                key.clone(),
                vctx,
                real_ctx.to_string(),
                peer_user_id.to_string(),
            );
        }
    }
    ctx_map.map_scoped(
        real_ctx.to_string(),
        peer_user_id.to_string(),
        group_id,
        client_scope,
    )
}

/// Build `HubExt` for an outbound message.
///
/// Sessions are scoped to `(vctx, vtoken)` so that each backend has its own independent
/// session namespace for the same WeChat conversation.
///
/// When `session_override` is provided (from a quote-reply), that session is used directly
/// instead of the current active session, so the message is routed to the correct conversation.
async fn build_hub_ext_for_vctx(
    store: &Store,
    vctx: &str,
    vtoken: &str,
    session_override: Option<String>,
) -> Option<HubExt> {
    let session_name = match session_override {
        Some(name) if !name.is_empty() => name,
        _ => store
            .get_active_session_name(vctx, vtoken)
            .await
            .ok()
            .unwrap_or_else(|| "default".to_string()),
    };

    let session_id = store
        .get_backend_session(vctx, vtoken, &session_name)
        .await
        .ok()
        .flatten()
        .and_then(|s| {
            let t = s.trim().to_string();
            (!t.is_empty()).then_some(t)
        });

    Some(HubExt {
        session_id,
        session_name: Some(session_name),
        cli_session_id: None,
    })
}

// ─── Static responder helpers ─────────────────────────────────────────────────

fn build_help_text() -> String {
    "iLink Hub 帮助\n\n\
     可用指令：\n\
     /status — 查看当前 Hub 状态\n\
     /list — 列出所有已注册的 AI 后端\n\
     /use <名称> — 切换到指定的 AI 后端\n\
     /help — 显示此帮助\n\n\
     Session 管理（同一后端下的多会话）：\n\
     /session list — 列出当前对话的所有 sessions\n\
     /session new <名称> [UUID] — 创建新 session（可选初始 UUID）\n\
     /session use <名称> — 切换到指定 session\n\
     /session delete <名称> — 删除指定 session\n\n\
     引用回复：引用某条机器人消息后发送的内容，会优先路由到发出该条消息的后端（或 Hub 指令结果），不必依赖当前 /use。\n\
     多后端时，各后端回复末尾可能带有「— 工作区名」展示行（仅**同时在线**的后端多于一个时默认追加；历史注册但离线的客户端不计入）。可用环境变量 ILINKHUB_OUTBOUND_ORIGIN_LABEL 强制关/开。\n\n\
     关于 iLink Hub：\n\
     本服务是一个消息路由中枢，可将您的微信消息转发给已接入的 AI 助手后端进行处理。\n\n\
     管理员接入指南：\n\
     1. 部署并启动 ilink-hub serve\n\
     2. 运行 ilink-hub register --name <名称> 注册后端\n\
     3. 将输出的 WEIXIN_TOKEN 配置到您的 AI 服务\n\
     4. AI 服务调用 /ilink/bot/getupdates 接收消息，并通过 /ilink/bot/sendmessage 回复"
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::InMemoryQueue;
    use crate::ilink::UpstreamClient;
    use crate::store::Store;

    #[test]
    fn poll_tracker_counts_concurrent_polls_and_releases_on_drop() {
        let tracker = Arc::new(PollTracker::default());

        let (c1, g1) = tracker.enter("vt-a");
        assert_eq!(c1, 1, "first poll is alone");

        let (c2, g2) = tracker.enter("vt-a");
        assert_eq!(c2, 2, "second concurrent poll on same vtoken detected");

        // A different vtoken is tracked independently.
        let (c_other, _g_other) = tracker.enter("vt-b");
        assert_eq!(c_other, 1);

        drop(g2);
        let (c3, _g3) = tracker.enter("vt-a");
        assert_eq!(
            c3, 2,
            "count drops when a guard is released, then rises again"
        );

        drop(g1);
        drop(_g3);
        // All vt-a guards released → entry removed; a fresh poll starts back at 1.
        let (c4, _g4) = tracker.enter("vt-a");
        assert_eq!(c4, 1);
    }

    /// Verify that concurrent calls to register_client_in_hub (registry → router lock order)
    /// never deadlock against each other or against route-reading.
    ///
    /// A deadlock would cause this test to hang and be killed by the tokio timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_register_and_route_does_not_deadlock() {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let state = HubState::new(upstream, Arc::new(store), queue, shutdown_rx);

        let mut handles = vec![];

        // Spawn tasks that repeatedly register clients (acquires registry write → router write).
        for i in 0..8 {
            let s = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                for j in 0..10 {
                    crate::server::pairing::register_client_in_hub(
                        &s,
                        format!("client-{i}-{j}"),
                        None,
                    )
                    .await;
                }
            }));
        }

        // Spawn tasks that repeatedly read the router (acquires router lock).
        for _ in 0..4 {
            let s = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let _ = s.router.lock().await.get_route("any_user");
                    tokio::task::yield_now().await;
                }
            }));
        }

        // All tasks must finish within 5 seconds — a deadlock would cause timeout.
        let timeout = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures_util::future::join_all(handles),
        )
        .await;
        assert!(
            timeout.is_ok(),
            "concurrent register+route timed out (possible deadlock)"
        );
    }
}
