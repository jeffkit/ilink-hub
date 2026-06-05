pub mod health;
pub mod queue;
pub mod registry;
pub mod router;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::ilink::types::{self, InboundMessage, SendMessageRequest};
use crate::ilink::UpstreamClient;
use crate::store::Store;

pub use health::spawn_health_checker;
pub use queue::{ClientQueue, ContextTokenMap, InMemoryQueue, MessageQueue};
pub use registry::{ClientInfo, ClientRegistry};
pub use router::{HubCommand, Router, RoutingDecision};

// ─── Metrics ──────────────────────────────────────────────────────────────────

pub struct Metrics {
    pub messages_dispatched: AtomicU64,
    pub messages_dropped: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_dispatched: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Shared Hub State ─────────────────────────────────────────────────────────

/// The core hub state, shared across all server handlers via `Arc<HubState>`.
pub struct HubState {
    pub upstream: Arc<UpstreamClient>,
    pub registry: RwLock<ClientRegistry>,
    /// Queue backend injected at construction time.
    pub queue: Arc<dyn MessageQueue>,
    pub ctx_map: Mutex<ContextTokenMap>,
    pub router: Mutex<Router>,
    pub store: Arc<Store>,
    pub metrics: Metrics,
}

impl HubState {
    /// `queue` — queue backend injected at construction time.
    pub fn new(
        upstream: Arc<UpstreamClient>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
    ) -> Arc<Self> {
        Arc::new(Self {
            upstream,
            registry: RwLock::new(ClientRegistry::new()),
            queue,
            ctx_map: Mutex::new(ContextTokenMap::default()),
            router: Mutex::new(Router::new(None)),
            store,
            metrics: Metrics::new(),
        })
    }
}

// ─── Message Dispatcher ───────────────────────────────────────────────────────

/// Spawns a background task that receives messages from the upstream broadcast
/// channel and dispatches them to the correct client queues.
pub fn spawn_dispatcher(state: Arc<HubState>, mut rx: broadcast::Receiver<InboundMessage>) {
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

async fn dispatch_message(state: Arc<HubState>, mut msg: InboundMessage) {
    let routing = {
        let router = state.router.lock().await;
        router.route(&msg)
    };

    match routing {
        RoutingDecision::HubInternal(cmd) => {
            handle_hub_command(state, msg, cmd).await;
        }
        RoutingDecision::ForwardTo(vtoken) => {
            // Replace real context_token with a virtual one (write-through to DB)
            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(msg.context_token.clone())
            };

            // Persist mapping to DB (best-effort, don't block dispatch on failure)
            let store = state.store.clone();
            let real_ctx = msg.context_token.clone();
            let vctx_clone = vctx.clone();
            tokio::spawn(async move {
                if let Err(e) = store.persist_context_token(&vctx_clone, &real_ctx).await {
                    warn!(error = %e, "failed to persist context_token mapping");
                }
            });

            msg.context_token = vctx;

            state
                .metrics
                .messages_dispatched
                .fetch_add(1, Ordering::Relaxed);
            match state.queue.push(&vtoken, msg).await {
                Ok(true) => {
                    // Overflow: oldest message was dropped to make room
                    state
                        .metrics
                        .messages_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(e) => {
                    error!(error = %e, vtoken = %vtoken, "failed to push message to queue");
                    state
                        .metrics
                        .messages_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        RoutingDecision::Broadcast => {
            let online = {
                let registry = state.registry.read().await;
                registry
                    .online_clients()
                    .iter()
                    .map(|c| c.vtoken.clone())
                    .collect::<Vec<_>>()
            };

            if online.is_empty() {
                warn!(from_user = %msg.from_user, "no online clients to dispatch to");
                state
                    .metrics
                    .messages_dropped
                    .fetch_add(1, Ordering::Relaxed);
                // Notify the user that no AI backends are available
                let reply = SendMessageRequest {
                    context_token: msg.context_token.clone(),
                    msg_type: types::msg_type::TEXT,
                    content: Some(
                        "⚠️ No AI backends are currently online.\n\
                         Use /list to see registered workspaces, or /status for hub info."
                            .to_string(),
                    ),
                    media_id: None,
                    extra: Default::default(),
                };
                if let Err(e) = state.upstream.send_message(reply).await {
                    error!(error = %e, "failed to send no-clients reply");
                }
                return;
            }

            for vtoken in &online {
                let vctx = {
                    let mut ctx_map = state.ctx_map.lock().await;
                    ctx_map.map(msg.context_token.clone())
                };

                let store = state.store.clone();
                let real_ctx = msg.context_token.clone();
                let vctx_clone = vctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = store.persist_context_token(&vctx_clone, &real_ctx).await {
                        warn!(error = %e, "failed to persist context_token mapping (broadcast)");
                    }
                });

                let mut msg_clone = msg.clone();
                msg_clone.context_token = vctx;
                state
                    .metrics
                    .messages_dispatched
                    .fetch_add(1, Ordering::Relaxed);
                match state.queue.push(vtoken, msg_clone).await {
                    Ok(true) => {
                        state
                            .metrics
                            .messages_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        error!(error = %e, vtoken = %vtoken, "failed to push broadcast message to queue");
                        state
                            .metrics
                            .messages_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

async fn handle_hub_command(state: Arc<HubState>, msg: InboundMessage, cmd: HubCommand) {
    let real_ctx = msg.context_token.clone();

    let reply_text = match cmd {
        HubCommand::List => {
            let registry = state.registry.read().await;
            let clients = registry.all_clients();
            if clients.is_empty() {
                "No clients registered yet.".to_string()
            } else {
                let mut lines = vec!["**Connected workspaces:**".to_string()];
                for c in clients {
                    let status = if c.online { "🟢" } else { "🔴" };
                    let label = c.label.as_deref().unwrap_or(&c.name);
                    lines.push(format!("{} `{}` — {}", status, c.name, label));
                }
                lines.push("\nUse `/use <name>` to switch.".to_string());
                lines.join("\n")
            }
        }
        HubCommand::UseClient(name) => {
            let registry = state.registry.read().await;
            if let Some(client) = registry.get_by_name(&name) {
                let vtoken = client.vtoken.clone();
                drop(registry);

                {
                    let mut router = state.router.lock().await;
                    router.set_route(&msg.from_user, vtoken.clone());
                }

                // Persist routing state to DB
                if let Err(e) = state.store.set_route(&msg.from_user, &vtoken).await {
                    warn!(error = %e, "failed to persist route to DB");
                }

                format!("✅ Switched to `{}`", name)
            } else {
                format!(
                    "❌ No client named `{}` found. Use `/list` to see available clients.",
                    name
                )
            }
        }
        HubCommand::Broadcast(text) => {
            // Re-dispatch as a broadcast message
            let broadcast_msg = InboundMessage {
                content: Some(text),
                ..msg.clone()
            };
            let online = {
                let registry = state.registry.read().await;
                registry
                    .online_clients()
                    .iter()
                    .map(|c| c.vtoken.clone())
                    .collect::<Vec<_>>()
            };
            for vtoken in &online {
                let vctx = {
                    let mut ctx_map = state.ctx_map.lock().await;
                    ctx_map.map(broadcast_msg.context_token.clone())
                };
                let mut m = broadcast_msg.clone();
                m.context_token = vctx;
                state
                    .metrics
                    .messages_dispatched
                    .fetch_add(1, Ordering::Relaxed);
                if let Err(e) = state.queue.push(vtoken, m).await {
                    error!(error = %e, vtoken = %vtoken, "failed to push hub broadcast message");
                }
            }
            format!("📡 Broadcast to {} client(s)", online.len())
        }
        HubCommand::Status => {
            let registry = state.registry.read().await;
            let online = registry.online_clients().len();
            let total = registry.all_clients().len();
            format!("iLink Hub — {}/{} clients online", online, total)
        }
    };

    // Send hub reply directly via upstream
    let send_req = SendMessageRequest {
        context_token: real_ctx,
        msg_type: 1, // TEXT
        content: Some(reply_text),
        media_id: None,
        extra: Default::default(),
    };

    if let Err(e) = state.upstream.send_message(send_req).await {
        error!(error = %e, "failed to send hub command reply");
    }
}
