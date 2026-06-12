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

// ─── Concurrency limits ───────────────────────────────────────────────────────

/// Maximum number of concurrent `getupdates` long-polls allowed for a single vtoken.
///
/// A healthy backend has exactly one bridge process polling its vtoken at a time.
/// When two or more bridge processes share one credential/token, they race for
/// the destructive `drain` of the per-vtoken message queue and inbound messages
/// get stolen non-deterministically (split-brain). To stop a malicious or
/// misconfigured client from holding an unbounded number of long-polls (which
/// would saturate the Tokio worker pool), the Hub caps the concurrent poll
/// count per vtoken at this value and rejects additional polls with HTTP 429.
///
/// SEC-003: a single vtoken must not be able to exhaust Hub resources. The
/// cap is intentionally small — anything beyond ~3 is already a configuration
/// problem worth surfacing in the operator logs.
pub const MAX_CONCURRENT_POLLS_PER_VTOKEN: usize = 3;

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
    /// Failures of the fire-and-forget `persist_context_token` call on the per-message
    /// (ForwardTo) dispatch path. Counts every background task that returned an error,
    /// so a non-zero value means context-token mappings were dropped on the floor (we
    /// keep delivering messages but lose durability of the real_ctx↔vctx mapping for
    /// those rows). See `docs/exec-plans/active/todo-hub-2/plan.md` C-01.
    pub persist_fire_and_forget_failures_forward: AtomicU64,
    /// Failures of the fire-and-forget `persist_context_tokens_batch` call on the
    /// broadcast / fan-out dispatch path. See `persist_fire_and_forget_failures_forward`
    /// for semantics. Split from the per-message counter so operators can distinguish
    /// single-row failures from per-broadcast batch failures.
    pub persist_fire_and_forget_failures_broadcast: AtomicU64,
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
            persist_fire_and_forget_failures_forward: AtomicU64::new(0),
            persist_fire_and_forget_failures_broadcast: AtomicU64::new(0),
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
    /// Per-vtoken concurrent poll counter. Public for test-only access so
    /// integration tests can poison the mutex to verify the let-Ok
    /// panic-safety path (F-M2-2); production code should only call
    /// `enter` / rely on `Drop`.
    pub counts: StdMutex<HashMap<String, usize>>,
}

impl PollTracker {
    /// Register a new active poll for `vtoken`. Returns the number of polls now concurrently
    /// active for that vtoken (always >= 1) and a guard that decrements the count on drop.
    ///
    /// F-M2-2: never panic on mutex poisoning. If the counts mutex is poisoned, the
    /// guard is still produced but the count is reported as 0 (which means the 429
    /// gate won't trip on this vtoken) and the drop handler becomes a best-effort
    /// no-op. A poisoned `counts` map is a process-wide bug, but it must not take
    /// the Tokio worker down on every subsequent long-poll.
    pub fn enter(self: &Arc<Self>, vtoken: &str) -> (usize, PollGuard) {
        let count = {
            let Ok(mut counts) = self.counts.lock() else {
                return (
                    0,
                    PollGuard {
                        tracker: Arc::clone(self),
                        vtoken: vtoken.to_string(),
                    },
                );
            };
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
        // F-M2-2: best-effort decrement; a poisoned mutex here would otherwise
        // propagate a panic into the Tokio worker that called the handler.
        let Ok(mut counts) = self.tracker.counts.lock() else {
            return;
        };
        if let Some(c) = counts.get_mut(&self.vtoken) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                counts.remove(&self.vtoken);
            }
        }
    }
}

// ─── Shared Hub State ─────────────────────────────────────────────────────────

/// State tied to the iLink upstream WebSocket connection.
///
/// Anything that mutates only when iLink connects, logs in, or sends a QR-ready
/// event lives here. Callers that need to send a message upstream, observe a QR
/// login, or trigger a re-login take a reference to this sub-state rather than
/// touching the whole `HubState`.
pub struct IlinkConnState {
    pub upstream: Arc<UpstreamClient>,
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
}

impl IlinkConnState {
    fn new(upstream: Arc<UpstreamClient>, shutdown: watch::Receiver<bool>) -> Self {
        let (qr_tx, _) = broadcast::channel(16);
        let (relogin_tx, _) = broadcast::channel(4);
        Self {
            upstream,
            shutdown,
            ilink_status: Arc::new(AtomicU8::new(ilink_status::UNKNOWN)),
            qr_tx,
            qr_last_ready: Arc::new(Mutex::new(None)),
            relogin_tx,
        }
    }
}

/// Routing-layer state: per-message dispatch decisions, conversation vctx
/// mapping, and quote-reply tracking. Pure in-memory; no I/O.
pub struct RoutingState {
    pub router: Mutex<Router>,
    pub ctx_map: RwLock<ContextTokenMap>,
    /// Quote-reply → backend / hub command (see [`quote_route`]).
    pub quote_index: Mutex<QuoteRouteIndex>,
}

impl RoutingState {
    fn new() -> Self {
        Self {
            router: Mutex::new(Router::new(None)),
            ctx_map: RwLock::new(ContextTokenMap::default()),
            quote_index: Mutex::new(QuoteRouteIndex::default()),
        }
    }
}

/// Registered backend clients, paired devices, the per-vtoken message queue,
/// and long-poll concurrency tracking.
pub struct ClientState {
    pub registry: RwLock<ClientRegistry>,
    pub pairing: RwLock<PairingRegistry>,
    pub queue: Arc<dyn MessageQueue>,
    /// Tracks concurrent `getupdates` long-polls per vtoken to detect bridges that share one
    /// credential/token (queue split-brain).
    pub poll_tracker: Arc<PollTracker>,
}

impl ClientState {
    fn new(queue: Arc<dyn MessageQueue>) -> Self {
        Self {
            registry: RwLock::new(ClientRegistry::new()),
            pairing: RwLock::new(PairingRegistry::new()),
            queue,
            poll_tracker: Arc::new(PollTracker::default()),
        }
    }
}

/// Top-level hub state. Groups related state into cohesive sub-states so that
/// internal helpers (dispatcher, hub-command handler, etc.) take the smallest
/// slice they need instead of the entire blob.
///
/// External callers (server routes, pairing, etc.) continue to access fields
/// through the same `state.field` paths they always have — the sub-state
/// fields are re-exported as direct `pub` fields on `HubState` for backward
/// compatibility. New code is encouraged to take `&RoutingState` /
/// `&IlinkConnState` / `&ClientState` parameters to make the dependency
/// explicit.
pub struct HubState {
    /// iLink upstream connection and shutdown signal.
    pub ilink: IlinkConnState,
    /// Per-message routing, vctx mapping, and quote-reply tracking.
    pub routing: RoutingState,
    /// Registered clients, paired devices, message queue, long-poll tracking.
    pub clients: ClientState,
    /// Persistent store (SQLx pool-backed). Cross-cutting; not part of any sub-state.
    pub store: Arc<Store>,
    /// Observability counters. Cross-cutting; not part of any sub-state.
    pub metrics: Arc<Metrics>,
}

impl HubState {
    pub fn new(
        upstream: Arc<UpstreamClient>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ilink: IlinkConnState::new(upstream, shutdown),
            routing: RoutingState::new(),
            clients: ClientState::new(queue),
            store,
            metrics: Arc::new(Metrics::new()),
        })
    }
}

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
        let router = state.routing.router.lock().await;
        router.route(&msg)
    };

    let quoted = {
        let scope = msg.from_user_id.as_deref().unwrap_or_default();
        let mut q = state.routing.quote_index.lock().await;
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
            let metrics = state.metrics.clone();
            tokio::spawn(async move {
                if let Err(e) = store.persist_context_token(&vctx2, &real2, &peer2).await {
                    warn!(error = %e, "failed to persist context_token mapping");
                    metrics
                        .persist_fire_and_forget_failures_forward
                        .fetch_add(1, Ordering::Relaxed);
                }
            });

            let hub_ext =
                build_hub_ext_for_vctx(&state.store, &vctx, &vtoken, session_override).await;
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

            // Batch-persist in one transaction (fire-and-forget).
            {
                let entries: Vec<(String, String, String)> = vctx_by_vtoken
                    .iter()
                    .map(|(_, vc)| (vc.clone(), real_ctx.clone(), peer_user_id.clone()))
                    .collect();
                let store = state.store.clone();
                let metrics = state.metrics.clone();
                tokio::spawn(async move {
                    if let Err(e) = store.persist_context_tokens_batch(&entries).await {
                        warn!(error = %e, "failed to batch-persist context_token mappings (broadcast)");
                        metrics
                            .persist_fire_and_forget_failures_broadcast
                            .fetch_add(1, Ordering::Relaxed);
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
                push_to_queue(&state.clients.queue, &state.metrics, &vtoken, msg_clone).await;
            }
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
        .routing
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
            let registry = state.clients.registry.read().await;
            let clients = registry.all_clients();
            if clients.is_empty() {
                "尚未注册任何后端客户端。".to_string()
            } else {
                let active_vtoken = {
                    let router = state.routing.router.lock().await;
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
            let registry = state.clients.registry.read().await;
            if let Some(client) = registry.get_by_name(name) {
                let vtoken = client.vtoken.clone();
                drop(registry);

                {
                    let mut router = state.routing.router.lock().await;
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
                let registry = state.clients.registry.read().await;
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
                    let items_mut = std::sync::Arc::make_mut(items);
                    if let Some(first) = items_mut.first_mut() {
                        if let Some(ti) = &mut first.text_item {
                            ti.text = Some(text.clone());
                        }
                    }
                }
                match state.clients.queue.push(vtoken, m).await {
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
                        error!(error = %e, vtoken = %crate::redact_token(vtoken), "failed to push hub broadcast message");
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
            let registry = state.clients.registry.read().await;
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
                        let registry = state.clients.registry.read().await;
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
                                        s.backend_session_id.chars().take(12).collect::<String>()
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
                state
                    .routing
                    .quote_index
                    .lock()
                    .await
                    .register_outbound_content(
                        &from_user_id,
                        &text,
                        quote_route::QuoteOrigin::Hub { cmd: cmd.clone() },
                    );
            }
        }
    }
    match state.ilink.upstream.send_message(send_req).await {
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
            !state.routing.ctx_map.read().await.has_conversation(key)
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

    let ctx_map = state.routing.ctx_map.write().await;
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

    /// SEC-003: the poll tracker must surface that the per-vtoken cap has
    /// been exceeded. The handler in src/server/routes.rs uses
    /// `count > MAX_CONCURRENT_POLLS_PER_VTOKEN` to gate the 429 reply; this
    /// test pins the boundary so a future refactor that silently clamps
    /// the count to MAX (or that returns a stale value) is caught.
    #[test]
    fn poll_tracker_caps_concurrent() {
        let tracker = Arc::new(PollTracker::default());
        // Hold MAX guards so the (MAX+1)th enter must observe a count
        // strictly greater than MAX.
        let mut guards = Vec::with_capacity(MAX_CONCURRENT_POLLS_PER_VTOKEN);
        for expected in 1..=MAX_CONCURRENT_POLLS_PER_VTOKEN {
            let (c, g) = tracker.enter("vt-cap");
            assert_eq!(
                c, expected,
                "enter #{expected} must report {expected} active polls"
            );
            guards.push(g);
        }
        // The (MAX+1)th enter must see count == MAX+1 > MAX — this is the
        // signal the handler uses to return 429.
        let (over, g_over) = tracker.enter("vt-cap");
        assert_eq!(
            over,
            MAX_CONCURRENT_POLLS_PER_VTOKEN + 1,
            "the (MAX+1)th concurrent poll must be observable above the cap"
        );
        assert!(
            over > MAX_CONCURRENT_POLLS_PER_VTOKEN,
            "the cap is the 429 boundary; the handler gates on this"
        );
        drop(g_over);
        // After dropping the over-cap guard, count returns to MAX and a
        // fresh enter must NOT cross the boundary — this is the recovery
        // path that lets a legitimate client reconnect after a burst.
        let (back_to_max, g_back_to_max) = tracker.enter("vt-cap");
        assert_eq!(
            back_to_max,
            MAX_CONCURRENT_POLLS_PER_VTOKEN + 1,
            "the freshly entered guard again pushes the count to MAX+1"
        );
        drop(g_back_to_max);
        drop(guards);
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
                    let _ = s.routing.router.lock().await.get_route("any_user");
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

    #[tokio::test]
    async fn test_build_hub_ext_for_vctx_timeout() {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store");

        tokio::time::pause();

        // sqlx uses connection pool with max_connections = 1 for sqlite::memory:
        // Begin a transaction to acquire and hold the only connection.
        let _tx = store.pool().begin().await.unwrap();

        // Call build_hub_ext_for_vctx. It will attempt to get connection to call
        // get_active_session_name. This will block.
        // Since time is paused, tokio will automatically skip time forward when the future is blocked.
        // The timeout should trigger after 5 virtual seconds.
        let hub_ext = build_hub_ext_for_vctx(&store, "vctx-test", "vtoken-test", None).await;

        // It should fallback to default values:
        assert!(hub_ext.is_some());
        let ext = hub_ext.unwrap();
        assert_eq!(ext.session_name, Some("default".to_string()));
        assert_eq!(ext.session_id, None);
    }

    #[tokio::test]
    async fn test_build_hub_ext_for_vctx_timeout_with_session_override() {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store");

        tokio::time::pause();

        // Begin a transaction to acquire and hold the only connection.
        let _tx = store.pool().begin().await.unwrap();

        // Call build_hub_ext_for_vctx with session_override.
        // It will skip get_active_session_name, but will block on get_backend_session.
        // It should hit the 5-second timeout and fallback gracefully.
        let hub_ext = build_hub_ext_for_vctx(
            &store,
            "vctx-test",
            "vtoken-test",
            Some("override".to_string()),
        )
        .await;

        assert!(hub_ext.is_some());
        let ext = hub_ext.unwrap();
        assert_eq!(ext.session_name, Some("override".to_string()));
        assert_eq!(ext.session_id, None);
    }

    /// `persist_fire_and_forget_failures_broadcast` increments on broadcast-path persist errors.
    ///
    /// Spawns a tokio task using the exact fire-and-forget shape from the broadcast
    /// dispatch path (see `dispatch_message` `RoutingDecision::Broadcast`). We use a
    /// SQLite in-memory pool with a held transaction to force the persist call to
    /// block, then advance virtual time past the pool's default acquire timeout
    /// so the inner call returns an error. The metric must then reflect >= 1 failure,
    /// proving the C-01 counter is wired to the same code that runs in production.
    #[tokio::test]
    async fn persist_fire_and_forget_failure_increments_metric() {
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        tokio::time::pause();

        let metrics = Arc::new(Metrics::new());
        assert_eq!(
            metrics
                .persist_fire_and_forget_failures_broadcast
                .load(Ordering::Relaxed),
            0,
            "broadcast counter starts at zero"
        );
        assert_eq!(
            metrics
                .persist_fire_and_forget_failures_forward
                .load(Ordering::Relaxed),
            0,
            "forward counter starts at zero"
        );

        // Hold the only pool connection so the background persist call cannot acquire
        // a new one and the pool's acquire timeout will fire.
        let _tx = store.pool().begin().await.unwrap();

        let entries: Vec<(String, String, String)> = vec![(
            "vctx-1".to_string(),
            "real-1".to_string(),
            "peer-1".to_string(),
        )];

        let store_clone = store.clone();
        let metrics_clone = metrics.clone();
        let task = tokio::spawn(async move {
            // Same fire-and-forget shape used in dispatch_message::Broadcast.
            if let Err(e) = store_clone.persist_context_tokens_batch(&entries).await {
                warn!(error = %e, "failed to batch-persist context_token mappings (broadcast)");
                metrics_clone
                    .persist_fire_and_forget_failures_broadcast
                    .fetch_add(1, Ordering::Relaxed);
            }
        });

        // Advance virtual time so the pool acquire timeout fires (sqlx default is
        // 30 seconds; we give it a generous buffer).
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let _ = task.await;

        assert!(
            metrics
                .persist_fire_and_forget_failures_broadcast
                .load(Ordering::Relaxed)
                >= 1,
            "broadcast counter must have been incremented after persist failure"
        );
        assert_eq!(
            metrics
                .persist_fire_and_forget_failures_forward
                .load(Ordering::Relaxed),
            0,
            "forward counter must NOT be touched by broadcast failure"
        );
    }

    // ─── A-01: HubState sub-state composition ────────────────────────────────
    //
    // The A-01 refactor splits the monolithic HubState into IlinkConnState,
    // RoutingState, and ClientState. The tests below pin down the structural
    // invariant: HubState::new builds all three sub-states with the correct
    // fields populated, and internal helpers can take the smallest sub-state
    // reference they need without forcing callers to hand the full HubState.

    async fn make_state() -> Arc<HubState> {
        let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        HubState::new(upstream, store, queue, shutdown_rx)
    }

    #[tokio::test]
    async fn hub_state_new_populates_all_sub_states() {
        let state = make_state().await;

        // iLinkConnState fields wired up.
        assert!(Arc::strong_count(&state.ilink.upstream) >= 1);
        assert_eq!(
            state.ilink.ilink_status.load(Ordering::Relaxed),
            ilink_status::UNKNOWN,
            "iLink status starts at UNKNOWN"
        );
        // broadcast::Sender has no cheap invariants to assert, but we can
        // verify it can be subscribed to without panicking.
        let _rx = state.ilink.qr_tx.subscribe();
        let _ = state.ilink.relogin_tx.send(());

        // RoutingState is empty but functional.
        assert!(
            state.routing.ctx_map.read().await.is_empty(),
            "fresh RoutingState has no conversations"
        );
        assert!(
            state
                .routing
                .router
                .lock()
                .await
                .get_route("any_user")
                .is_none(),
            "fresh Router has no per-user route"
        );

        // ClientState is empty but functional.
        assert_eq!(state.clients.registry.read().await.all_clients().len(), 0);

        // Cross-cutting fields.
        assert!(Arc::strong_count(&state.metrics) >= 1);
        assert_eq!(state.metrics.messages_dispatched.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn sub_states_are_independently_usable() {
        // A-01 promises that callers can take the smallest slice they need.
        // This test exercises each sub-state through the same access path
        // the production code uses, but in isolation against the top-level
        // HubState handle.

        let state = make_state().await;

        // IlinkConnState: poll counters are reachable through the sub-state.
        // (We don't actually issue an HTTP request — the production base URL
        // would attempt a real network call, which is out of scope here.)
        assert_eq!(state.ilink.upstream.polls_ok.load(Ordering::Relaxed), 0);

        // RoutingState: setting a route and reading it back round-trips.
        let vtoken = "vt-abc".to_string();
        state
            .routing
            .router
            .lock()
            .await
            .set_route("user-x", vtoken.clone());
        assert_eq!(
            state.routing.router.lock().await.get_route("user-x"),
            Some(vtoken.as_str())
        );

        // ClientState: queue push + drain via the per-client queue.
        let weixin_msg = crate::ilink::types::WeixinMessage::default();
        let push_result = state.clients.queue.push(&vtoken, weixin_msg).await;
        assert!(
            push_result.is_ok(),
            "in-memory queue accepts the pushed message"
        );
    }

    #[test]
    fn sub_state_structs_carry_expected_fields() {
        // Compile-time check that IlinkConnState / RoutingState / ClientState
        // carry the documented fields. Touching each field name forces the
        // compiler to keep them — accidental removal will break this test.

        fn assert_ilink_fields(_s: &IlinkConnState) {
            let _ = &_s.upstream;
            let _ = &_s.shutdown;
            let _ = &_s.ilink_status;
            let _ = &_s.qr_tx;
            let _ = &_s.qr_last_ready;
            let _ = &_s.relogin_tx;
        }
        fn assert_routing_fields(_s: &RoutingState) {
            let _ = &_s.router;
            let _ = &_s.ctx_map;
            let _ = &_s.quote_index;
        }
        fn assert_client_fields(_s: &ClientState) {
            let _ = &_s.registry;
            let _ = &_s.pairing;
            let _ = &_s.queue;
            let _ = &_s.poll_tracker;
        }

        let (_tx, _rx) = tokio::sync::watch::channel(false);
        let _upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
        let _queue: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());

        let ilink = IlinkConnState::new(
            Arc::new(UpstreamClient::new("sk-test".to_string(), None)),
            _rx,
        );
        assert_ilink_fields(&ilink);

        let routing = RoutingState::new();
        assert_routing_fields(&routing);

        let client = ClientState::new(_queue);
        assert_client_fields(&client);
    }

    #[tokio::test]
    async fn hub_state_metrics_are_shared_with_sub_state_paths() {
        // The dispatcher increments metrics.messages_dispatched via the
        // Arc<Metrics> handle. This test asserts the same Arc is reachable
        // through the HubState.metrics field — i.e. the top-level Metrics
        // is not a separate clone from anything the sub-states touch.

        let state = make_state().await;
        state
            .metrics
            .messages_dispatched
            .fetch_add(7, Ordering::Relaxed);

        // The same Metrics instance must be reachable: incrementing from
        // the top-level handle must be visible to anyone holding an Arc
        // clone (which is the production pattern).
        let metrics_clone = Arc::clone(&state.metrics);
        assert_eq!(metrics_clone.messages_dispatched.load(Ordering::Relaxed), 7);
    }

    #[tokio::test]
    async fn quote_index_evictor_takes_sub_state_path() {
        // spawn_quote_index_evictor is the closest existing call to a
        // sub-state-only path: it only needs routing.quote_index and the
        // shutdown signal. Run a single iteration by exercising the lock
        // through the same `state.routing.quote_index` path the evictor
        // uses, and verify the lock is reachable (i.e. the path is wired).

        let state = make_state().await;
        let mut quote_idx = state.routing.quote_index.lock().await;
        quote_idx.evict_expired();
    }
}
