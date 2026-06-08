pub mod health;
pub mod outbound_label;
pub mod pairing;
pub mod queue;
pub mod quote_route;
pub mod registry;
pub mod router;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};
use crate::ilink::UpstreamClient;
use crate::store::Store;

pub use health::spawn_health_checker;
pub use outbound_label::{
    append_outbound_origin_footer_to_first_text_item, format_outbound_origin_line,
    should_append_outbound_origin_label,
};
pub use pairing::PairingRegistry;
pub use queue::{ClientQueue, ContextTokenMap, InMemoryQueue, MessageQueue};
pub use quote_route::{merge_routing_with_quote, QuoteRouteIndex};
pub use registry::{ClientInfo, ClientRegistry};
pub use router::{HubCommand, Router, RoutingDecision};

// ─── Metrics ──────────────────────────────────────────────────────────────────

pub struct Metrics {
    pub messages_dispatched: AtomicU64,
    pub messages_dropped: AtomicU64,
    /// User-side (or command) messages taken from upstream and passed into routing
    /// (excludes bot-side echo copies with `message_type == 2`).
    pub upstream_user_messages: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_dispatched: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            upstream_user_messages: AtomicU64::new(0),
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
    /// Quote-reply → backend / hub command (see [`quote_route`]).
    pub quote_index: Mutex<QuoteRouteIndex>,
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
            quote_index: Mutex::new(QuoteRouteIndex::default()),
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
    // Bot-side copies from upstream (used to correlate outbound client_id → item msg_id).
    if msg.message_type == Some(2) {
        let mut q = state.quote_index.lock().await;
        q.observe_upstream_bot_message(&msg);
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
        let mut q = state.quote_index.lock().await;
        q.resolve_user_quote(&msg)
    };
    let routing = merge_routing_with_quote(routing, quoted);

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

            let hub_ext = build_hub_ext_for_vctx(&state.store, &vctx).await;

            msg.context_token = Some(vctx);
            msg.ilink_hub_ext = hub_ext;

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
                let hub_ext = build_hub_ext_for_vctx(&state.store, &vctx).await;
                msg_clone.context_token = Some(vctx);
                msg_clone.ilink_hub_ext = hub_ext;
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
                let hub_ext = build_hub_ext_for_vctx(&state.store, &vctx).await;
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

        HubCommand::SessionList => {
            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(real_ctx.clone(), from_user_id.clone())
            };
            let active = state
                .store
                .get_active_session_name(&vctx)
                .await
                .unwrap_or_else(|_| "default".to_string());
            match state.store.list_backend_sessions(&vctx).await {
                Ok(sessions) if sessions.is_empty() => {
                    format!("当前对话尚无 session 记录。\n发送 `/session new <名称>` 创建一个 session。")
                }
                Ok(sessions) => {
                    let mut lines = vec!["**当前对话的 sessions：**".to_string()];
                    for s in &sessions {
                        let marker = if s.session_name == active { " ✅" } else { "" };
                        let uuid_hint = if s.backend_session_id.is_empty() {
                            "（尚无 UUID，下次对话时由后端写入）".to_string()
                        } else {
                            format!("`{}`", &s.backend_session_id[..s.backend_session_id.len().min(12)])
                        };
                        lines.push(format!("• `{}`{} — {}", s.session_name, marker, uuid_hint));
                    }
                    lines.push(format!("\n当前活跃：`{}`", active));
                    lines.push("\n用 `/session use <名称>` 切换，`/session new <名称>` 新建。".to_string());
                    lines.join("\n")
                }
                Err(e) => format!("❌ 查询 session 失败：{e}"),
            }
        }

        HubCommand::SessionNew(ref session_name, ref initial_uuid) => {
            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(real_ctx.clone(), from_user_id.clone())
            };
            match state
                .store
                .set_backend_session(&vctx, session_name, initial_uuid)
                .await
            {
                Ok(()) => {
                    if initial_uuid.is_empty() {
                        format!(
                            "✅ 已创建 session `{session_name}`。\n下次对话时后端会写入 UUID。\n发送 `/session use {session_name}` 切换到此 session。"
                        )
                    } else {
                        format!(
                            "✅ 已创建 session `{session_name}`，UUID: `{}`。\n发送 `/session use {session_name}` 切换到此 session。",
                            &initial_uuid[..initial_uuid.len().min(12)]
                        )
                    }
                }
                Err(e) => format!("❌ 创建 session 失败：{e}"),
            }
        }

        HubCommand::SessionUse(ref session_name) => {
            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(real_ctx.clone(), from_user_id.clone())
            };
            // Ensure the session exists (auto-create slot with empty UUID if not)
            let ensure_result: Result<(), String> =
                match state.store.get_backend_session(&vctx, session_name).await {
                    Ok(None) => state
                        .store
                        .set_backend_session(&vctx, session_name, "")
                        .await
                        .map_err(|e| format!("❌ 创建 session slot 失败：{e}")),
                    Err(e) => Err(format!("❌ 查询 session 失败：{e}")),
                    Ok(Some(_)) => Ok(()),
                };
            match ensure_result {
                Err(msg) => msg,
                Ok(()) => {
                    match state
                        .store
                        .set_active_session_name(&vctx, session_name)
                        .await
                    {
                        Ok(()) => format!("✅ 已切换到 session `{session_name}`"),
                        Err(e) => format!("❌ 切换 session 失败：{e}"),
                    }
                }
            }
        }

        HubCommand::SessionDelete(ref session_name) => {
            let vctx = {
                let mut ctx_map = state.ctx_map.lock().await;
                ctx_map.map(real_ctx.clone(), from_user_id.clone())
            };
            let active = state
                .store
                .get_active_session_name(&vctx)
                .await
                .unwrap_or_else(|_| "default".to_string());
            if *session_name == active {
                format!(
                    "❌ 无法删除当前活跃的 session `{session_name}`。\n请先用 `/session use <其他名称>` 切换后再删除。"
                )
            } else {
                match state
                    .store
                    .delete_backend_session(&vctx, session_name)
                    .await
                {
                    Ok(true) => format!("✅ 已删除 session `{session_name}`"),
                    Ok(false) => format!("❌ 未找到 session `{session_name}`"),
                    Err(e) => format!("❌ 删除 session 失败：{e}"),
                }
            }
        }
    };

    debug!(to = %from_user_id, "sending hub command reply");
    let mut send_req = SendMessageRequest::reply(real_ctx, reply_text, &from_user_id);
    if let Some(m) = &mut send_req.msg {
        m.ensure_outbound();
        if let Some(cid) = m.client_id.as_deref().filter(|s| !s.is_empty()) {
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
                let mut q = state.quote_index.lock().await;
                q.register_pending_hub(cid, cmd.clone());
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

/// Build `HubExt` for an outbound message by looking up the active session for the given vctx.
async fn build_hub_ext_for_vctx(store: &Store, vctx: &str) -> Option<HubExt> {
    let session_name = store
        .get_active_session_name(vctx)
        .await
        .ok()
        .unwrap_or_else(|| "default".to_string());

    let session_id = store
        .get_backend_session(vctx, &session_name)
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
     多后端时，各后端回复末尾可能带有「— 工作区名」展示行（仅一个注册后端时默认不加）；可用环境变量 ILINKHUB_OUTBOUND_ORIGIN_LABEL 强制关/开。\n\n\
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
