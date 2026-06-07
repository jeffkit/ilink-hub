pub mod health;
pub mod pairing;
pub mod queue;
pub mod registry;
pub mod router;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::ilink::types::{SendMessageRequest, WeixinMessage};
use crate::ilink::UpstreamClient;
use crate::store::Store;

pub use health::spawn_health_checker;
pub use pairing::PairingRegistry;
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

pub struct HubState {
    pub upstream: Arc<UpstreamClient>,
    pub registry: RwLock<ClientRegistry>,
    pub pairing: RwLock<PairingRegistry>,
    pub queue: Arc<dyn MessageQueue>,
    pub ctx_map: Mutex<ContextTokenMap>,
    pub router: Mutex<Router>,
    pub store: Arc<Store>,
    pub metrics: Metrics,
}

impl HubState {
    pub fn new(
        upstream: Arc<UpstreamClient>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
    ) -> Arc<Self> {
        Arc::new(Self {
            upstream,
            registry: RwLock::new(ClientRegistry::new()),
            pairing: RwLock::new(PairingRegistry::new()),
            queue,
            ctx_map: Mutex::new(ContextTokenMap::default()),
            router: Mutex::new(Router::new(None)),
            store,
            metrics: Metrics::new(),
        })
    }
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

async fn dispatch_message(state: Arc<HubState>, mut msg: WeixinMessage) {
    let routing = {
        let router = state.router.lock().await;
        router.route(&msg)
    };

    match routing {
        RoutingDecision::HubInternal(cmd) => {
            handle_hub_command(state, msg, cmd).await;
        }
        RoutingDecision::ForwardTo(vtoken) => {
            let real_ctx = match msg.context_token.clone() {
                Some(ctx) if !ctx.is_empty() => ctx,
                _ => {
                    warn!("message has no context_token, skipping dispatch");
                    return;
                }
            };

            let peer_user_id = msg.from_user_id.clone().unwrap_or_default();

            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(real_ctx.clone(), peer_user_id.clone())
            };

            let store = state.store.clone();
            let vctx_clone = vctx.clone();
            tokio::spawn(async move {
                if let Err(e) = store
                    .persist_context_token(&vctx_clone, &real_ctx, &peer_user_id)
                    .await
                {
                    warn!(error = %e, "failed to persist context_token mapping");
                }
            });

            msg.context_token = Some(vctx);

            state
                .metrics
                .messages_dispatched
                .fetch_add(1, Ordering::Relaxed);
            match state.queue.push(&vtoken, msg).await {
                Ok(true) => {
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

                // Notify the user that no AI backends are available
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

            for vtoken in &online {
                let vctx = {
                    let mut ctx_map = state.ctx_map.lock().await;
                    ctx_map.map(real_ctx.clone(), peer_user_id.clone())
                };

                let store = state.store.clone();
                let vctx_clone = vctx.clone();
                let real_ctx_clone = real_ctx.clone();
                let peer_clone = peer_user_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = store
                        .persist_context_token(&vctx_clone, &real_ctx_clone, &peer_clone)
                        .await
                    {
                        warn!(error = %e, "failed to persist context_token mapping (broadcast)");
                    }
                });

                let mut msg_clone = msg.clone();
                msg_clone.context_token = Some(vctx);
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
                        error!(error = %e, vtoken = %vtoken, "failed to push broadcast message");
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

                format!("✅ Switched to `{}`", name)
            } else {
                format!(
                    "❌ No client named `{}` found. Use `/list` to see available clients.",
                    name
                )
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
                let vctx = {
                    let mut ctx_map = state.ctx_map.lock().await;
                    ctx_map.map(real_ctx.clone(), from_user_id.clone())
                };
                let mut m = msg.clone();
                m.context_token = Some(vctx.clone());
                // Replace text content in item_list
                if let Some(items) = &mut m.item_list {
                    if let Some(first) = items.first_mut() {
                        if let Some(ti) = &mut first.text_item {
                            ti.text = Some(text.clone());
                        }
                    }
                }
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
            format!("iLink Hub 状态：{}/{} 个客户端在线", online, total)
        }
        HubCommand::Help => build_help_text(),
    };

    debug!(to = %from_user_id, "sending hub command reply");
    let send_req = SendMessageRequest::reply(real_ctx, reply_text, &from_user_id);
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

// ─── Static responder helpers ─────────────────────────────────────────────────

fn build_help_text() -> String {
    "iLink Hub 帮助\n\n\
     可用指令：\n\
     /status — 查看当前 Hub 状态\n\
     /list — 列出所有已注册的 AI 后端\n\
     /use <名称> — 切换到指定的 AI 后端\n\
     /help — 显示此帮助\n\n\
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
        return "未识别的指令。发送 /help 查看可用指令。".to_string();
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
