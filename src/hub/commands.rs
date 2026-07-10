//! Hub command handling: the `/list`, `/use`, `/status`, `/help`, `/session …`
//! and broadcast commands the user can send as WeChat messages.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, error, warn};

use crate::ilink::types::{SendMessageRequest, WeixinMessage};

// Hub-internal items (HubState, HubCommand, the `messages`/`quote_route` modules,
// …) plus the dispatch helpers via `super::dispatch::*`.
use super::*;

/// Resolve the vctx and currently routed vtoken for a Hub command from a given user.
/// Returns `None` if no backend is selected (broadcasts a NO_BACKEND message via the caller).
async fn resolve_vctx_and_vtoken(
    state: &HubState,
    real_ctx: &str,
    from_user_id: &str,
    group_id: Option<&str>,
) -> (String, Option<String>) {
    let vctx =
        super::dispatch::resolve_vctx_for_message(state, real_ctx, from_user_id, group_id, None)
            .await;
    let vtoken = state
        .routing
        .router
        .lock()
        .await
        .get_route(from_user_id)
        .map(str::to_string);
    (vctx, vtoken)
}

pub(super) async fn handle_hub_command(state: Arc<HubState>, msg: WeixinMessage, cmd: HubCommand) {
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
        HubCommand::List => handle_cmd_list(&state, &from_user_id).await,
        HubCommand::UseClient(ref name) => handle_cmd_use(&state, &from_user_id, name).await,
        HubCommand::Broadcast(ref text) => {
            handle_cmd_broadcast(&state, &from_user_id, &real_ctx, &msg, text).await
        }
        HubCommand::Status => handle_cmd_status(&state).await,
        HubCommand::Help => handle_cmd_help(),
        HubCommand::SessionList => {
            handle_cmd_session_list(&state, &from_user_id, &real_ctx, msg.group_id.as_deref()).await
        }
        HubCommand::SessionNew(ref session_name, ref initial_uuid) => {
            handle_cmd_session_new(
                &state,
                &from_user_id,
                &real_ctx,
                msg.group_id.as_deref(),
                session_name,
                initial_uuid,
            )
            .await
        }
        HubCommand::SessionUse(ref session_name) => {
            handle_cmd_session_use(
                &state,
                &from_user_id,
                &real_ctx,
                msg.group_id.as_deref(),
                session_name,
            )
            .await
        }
        HubCommand::SessionDelete(ref session_name) => {
            handle_cmd_session_delete(
                &state,
                &from_user_id,
                &real_ctx,
                msg.group_id.as_deref(),
                session_name,
            )
            .await
        }
    };

    debug!(to = %from_user_id, "sending hub command reply");
    let send_req = SendMessageRequest::reply(real_ctx, reply_text, &from_user_id);
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

pub(super) async fn handle_cmd_list(state: &HubState, from_user_id: &str) -> String {
    // Lock order: never hold registry and router at the same time.
    // Clone client list under the read lock, then release before taking router
    // (same pattern as `handle_cmd_use`). Holding both in opposite order from
    // `load_clients_from_db` (router → registry) would AB-BA deadlock.
    let clients = {
        let registry = state.clients.registry.read().await;
        let mut clients: Vec<_> = registry.all_clients().into_iter().cloned().collect();
        // Sort by name so the 1-based index shown here matches `get_by_alias`.
        clients.sort_by(|a, b| a.name.cmp(&b.name));
        clients
    };
    if clients.is_empty() {
        "尚未注册任何后端客户端。".to_string()
    } else {
        let active_vtoken = {
            let router = state.routing.router.lock().await;
            router.get_route(from_user_id).map(str::to_string)
        };
        let active_name = active_vtoken.as_deref().and_then(|vt| {
            clients
                .iter()
                .find(|c| c.vtoken == vt)
                .map(|c| c.name.as_str())
        });
        let mut lines = vec!["**已注册的后端：**".to_string()];
        for (i, c) in clients.iter().enumerate() {
            let status = if c.online { "🟢" } else { "🔴" };
            let label = c.label.as_deref().unwrap_or(&c.name);
            let selected = if active_name == Some(c.name.as_str()) {
                " ✅"
            } else {
                ""
            };
            lines.push(format!(
                "{} {}. `{}`{} — {}",
                status,
                i + 1,
                c.name,
                selected,
                label
            ));
        }
        match active_name {
            Some(name) => lines.push(format!("\n当前选中：`{}`", name)),
            None => lines.push("\n当前未选中（广播模式）".to_string()),
        }
        lines.push(
            "用 `/use <名称或序号>`（或 `/u <名称或序号>`）切换后端，或发送 `@<名称或序号> <消息>` 直接发起临时会话。"
                .to_string(),
        );
        lines.join("\n")
    }
}

pub(super) async fn handle_cmd_use(state: &HubState, from_user_id: &str, name: &str) -> String {
    let registry = state.clients.registry.read().await;
    if let Some(client) = registry.get_by_alias(name) {
        let vtoken = client.vtoken.clone();
        let resolved_name = client.name.clone();
        drop(registry);

        if let Err(e) = state.store.set_route(from_user_id, &vtoken).await {
            warn!(error = %e, "failed to persist route to DB");
            format!(
                "⚠️ 切换到 `{}` 失败（数据库写入错误），请重试",
                resolved_name
            )
        } else {
            let mut router = state.routing.router.lock().await;
            router.set_route(from_user_id, vtoken.clone());
            format!("✅ 已切换到 `{}`", resolved_name)
        }
    } else {
        format!(
            "❌ 未找到名为 `{}` 的后端。用 `/list`（或 `/ls`）查看可用后端（支持用序号，如 `/use 1`）。",
            name
        )
    }
}

pub(super) async fn handle_cmd_broadcast(
    state: &HubState,
    from_user_id: &str,
    real_ctx: &str,
    msg: &WeixinMessage,
    text: &str,
) -> String {
    let online = {
        let registry = state.clients.registry.read().await;
        registry
            .online_clients()
            .iter()
            .map(|c| c.vtoken.clone())
            .collect::<Vec<_>>()
    };
    for vtoken in &online {
        let vctx = super::dispatch::resolve_vctx_for_message(
            state,
            real_ctx,
            from_user_id,
            msg.group_id.as_deref(),
            None,
        )
        .await;
        let mut m = msg.clone();
        let hub_ext =
            super::dispatch::build_hub_ext_for_vctx(&state.store, &vctx, vtoken, None).await;
        m.context_token = Some(vctx.clone());
        m.ilink_hub_ext = hub_ext;
        if let Some(items) = &mut m.item_list {
            let items_mut = std::sync::Arc::make_mut(items);
            if let Some(first) = items_mut.first_mut() {
                if let Some(ti) = &mut first.text_item {
                    ti.text = Some(text.to_string());
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

pub(super) async fn handle_cmd_status(state: &HubState) -> String {
    let (online, total, online_clients) = {
        let registry = state.clients.registry.read().await;
        let all = registry.all_clients();
        let online = registry.online_clients().len();
        let total = all.len();
        let online_clients: Vec<(String, String)> = all
            .iter()
            .filter(|c| c.online)
            .map(|c| (c.name.clone(), c.vtoken.clone()))
            .collect();
        (online, total, online_clients)
    };
    let vtokens: Vec<String> = online_clients.iter().map(|(_, vt)| vt.clone()).collect();
    let all_sessions_map = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        state.store.get_all_session_entries_per_vtoken(&vtokens),
    )
    .await
    .unwrap_or_else(|_| {
        warn!("get_all_session_entries_per_vtoken timed out in /status");
        Ok(std::collections::HashMap::new())
    })
    .unwrap_or_default();
    let client_sessions: Vec<(String, Vec<crate::store::SessionStatusEntry>)> = online_clients
        .into_iter()
        .map(|(name, vtoken)| {
            let sessions = all_sessions_map.get(&vtoken).cloned().unwrap_or_default();
            (name, sessions)
        })
        .collect();
    messages::hub_status(online, total, &client_sessions)
}

pub(super) fn handle_cmd_help() -> String {
    build_help_text()
}

pub(super) async fn handle_cmd_session_list(
    state: &HubState,
    from_user_id: &str,
    real_ctx: &str,
    group_id: Option<&str>,
) -> String {
    let (vctx, vtoken) = resolve_vctx_and_vtoken(state, real_ctx, from_user_id, group_id).await;
    match vtoken {
        None => messages::NO_BACKEND.to_string(),
        Some(vtoken) => {
            let backend_name = {
                let registry = state.clients.registry.read().await;
                registry
                    .all_clients()
                    .into_iter()
                    .find(|c| c.vtoken == vtoken)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| crate::redact_token(&vtoken))
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
                    let mut lines = vec![format!("**后端 `{backend_name}` 的 sessions：**")];
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
                        lines.push(format!("• `{}`{} — {}", s.session_name, marker, uuid_hint));
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

pub(super) async fn handle_cmd_session_new(
    state: &HubState,
    from_user_id: &str,
    real_ctx: &str,
    group_id: Option<&str>,
    session_name: &str,
    initial_uuid: &str,
) -> String {
    let (vctx, vtoken) = resolve_vctx_and_vtoken(state, real_ctx, from_user_id, group_id).await;
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
                        Err(e) => messages::session_new_created_switch_failed(session_name, &e),
                    }
                }
                Err(e) => messages::session_new_failed(&e),
            }
        }
    }
}

pub(super) async fn handle_cmd_session_use(
    state: &HubState,
    from_user_id: &str,
    real_ctx: &str,
    group_id: Option<&str>,
    session_name: &str,
) -> String {
    let (vctx, vtoken) = resolve_vctx_and_vtoken(state, real_ctx, from_user_id, group_id).await;
    match vtoken {
        None => messages::NO_BACKEND.to_string(),
        Some(vtoken) => {
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

pub(super) async fn handle_cmd_session_delete(
    state: &HubState,
    from_user_id: &str,
    real_ctx: &str,
    group_id: Option<&str>,
    session_name: &str,
) -> String {
    let (vctx, vtoken) = resolve_vctx_and_vtoken(state, real_ctx, from_user_id, group_id).await;
    match vtoken {
        None => messages::NO_BACKEND.to_string(),
        Some(vtoken) => {
            let active = state
                .store
                .get_active_session_name(&vctx, &vtoken)
                .await
                .unwrap_or_else(|_| "default".to_string());
            if session_name == active {
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

// ─── Static responder helpers ─────────────────────────────────────────────────

fn build_help_text() -> String {
    "iLink Hub 帮助\n\n\
     可用指令（括号内为缩写）：\n\
     /status（/s）— 查看当前 Hub 状态\n\
     /list（/ls）— 列出所有已注册的 AI 后端\n\
     /use <名称或序号>（/u <名称或序号>）— 切换到指定的 AI 后端\n\
     /help（/h）— 显示此帮助\n\n\
     Session 管理（同一后端下的多会话）：\n\
     /session list（/sl）— 列出当前对话的所有 sessions\n\
     /session new <名称> [UUID]（/sn <名称>）— 创建新 session（可选初始 UUID）\n\
     /session use <名称>（/su <名称>）— 切换到指定 session\n\
     /session delete <名称>（/sd <名称>）— 删除指定 session\n\n\
     快捷 @ 后端：发送 `@<名称或序号> <消息>` 可临时在该后端上**新建一个会话**并发送此消息，不会改变你当前 /use 的后端和活跃 session（与引用回复类似的临时操作）。名称与 /use 使用的名称一致（可用 /list 查看，序号即 /list 中的编号）；名称取第一个空格之前的部分，其余为消息内容。需要继续这个临时会话时，引用它的回复即可。\n\n\
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{AdminConfig, HubState, InMemoryQueue};
    use crate::ilink::types::WeixinMessage;
    use std::sync::Arc;

    async fn make_hub_state() -> Arc<HubState> {
        make_hub_state_with_upstream(crate::hub::tests::MockUpstream::returning_ok()).await
    }

    async fn make_hub_state_with_upstream(
        upstream: Arc<dyn crate::ilink::UpstreamSink>,
    ) -> Arc<HubState> {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        let queue: Arc<dyn crate::MessageQueue> = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        HubState::new(
            upstream,
            Arc::new(store),
            queue,
            shutdown_rx,
            "test-relay-secret".to_string(),
            AdminConfig::from_env(),
        )
    }

    /// M1-1: handle_cmd_broadcast with no online clients must return the correct
    /// count string. Catches the mutant that replaces the whole function body
    /// with String::new() or "xyzzy".
    #[tokio::test]
    async fn broadcast_to_no_online_clients_returns_zero_count() {
        let state = make_hub_state().await;
        let msg = WeixinMessage::default();
        let result = handle_cmd_broadcast(&state, "user1", "ctx-abc", &msg, "hello").await;
        assert_eq!(
            result, "📡 Broadcast to 0 client(s)",
            "broadcast with no online clients must report 0"
        );
    }

    /// M1-2: handle_cmd_status with no clients must return a non-empty hub
    /// status string. Catches the mutant that replaces the function with String::new().
    #[tokio::test]
    async fn status_with_no_clients_returns_hub_status_string() {
        let state = make_hub_state().await;
        let result = handle_cmd_status(&state).await;
        assert!(
            result.contains("iLink Hub 状态：0/0"),
            "status with no clients must contain '0/0', got: {result:?}"
        );
    }

    /// M1-3: handle_cmd_session_list with no backend selected (no routing
    /// entry for the user) must return the NO_BACKEND message.
    /// Catches the == → != mutant on vtoken match and sessions.is_empty() → false.
    #[tokio::test]
    async fn session_list_with_no_backend_returns_no_backend_message() {
        let state = make_hub_state().await;
        let result = handle_cmd_session_list(&state, "user-no-backend", "ctx-xyz", None).await;
        assert_eq!(
            result,
            messages::NO_BACKEND,
            "session list with no backend must return NO_BACKEND message"
        );
    }

    /// M1-4: handle_hub_command with an empty context_token must return early
    /// without calling upstream. Catches `!ctx.is_empty()` → true (would send).
    #[tokio::test]
    async fn handle_hub_command_with_empty_context_token_returns_early() {
        let mock = crate::hub::tests::MockUpstream::returning_ok();
        let mock_ref = Arc::clone(&mock);
        let state = make_hub_state_with_upstream(mock).await;
        let msg = WeixinMessage {
            context_token: Some(String::new()),
            from_user_id: Some("user1".to_string()),
            ..Default::default()
        };
        handle_hub_command(Arc::clone(&state), msg, HubCommand::Help).await;
        assert_eq!(
            mock_ref.polls_ok(),
            0,
            "empty context_token must not call send_message"
        );
    }

    /// M1-4b: missing context_token (None) must also skip upstream send.
    #[tokio::test]
    async fn handle_hub_command_with_none_context_token_skips_send() {
        let mock = crate::hub::tests::MockUpstream::returning_ok();
        let mock_ref = Arc::clone(&mock);
        let state = make_hub_state_with_upstream(mock).await;
        let msg = WeixinMessage {
            context_token: None,
            from_user_id: Some("user1".to_string()),
            ..Default::default()
        };
        handle_hub_command(Arc::clone(&state), msg, HubCommand::Help).await;
        assert_eq!(mock_ref.polls_ok(), 0);
    }

    /// M1-5: handle_hub_command with a valid context_token must call send_message
    /// exactly once via MockUpstream. Verifies the upstream is called regardless of
    /// whether resp.ret is zero or non-zero.
    #[tokio::test]
    async fn handle_hub_command_calls_send_message_once() {
        let mock = crate::hub::tests::MockUpstream::returning_ok();
        let mock_ref = Arc::clone(&mock);
        let state = make_hub_state_with_upstream(mock).await;
        let msg = WeixinMessage {
            context_token: Some("ctx-valid-123".to_string()),
            from_user_id: Some("user1".to_string()),
            ..Default::default()
        };
        handle_hub_command(Arc::clone(&state), msg, HubCommand::Help).await;
        assert_eq!(
            mock_ref.polls_ok(),
            1,
            "send_message must be called exactly once for a valid context"
        );
    }

    /// M1-6: handle_hub_command with a valid context_token and MockUpstream
    /// returning non-zero ret must still complete without panicking.
    /// Catches the send_message → Ok(()) no-op mutant.
    #[tokio::test]
    async fn handle_hub_command_completes_when_upstream_returns_err_ret() {
        let mock = crate::hub::tests::MockUpstream::returning_err(1, "mock rejected");
        let mock_ref = Arc::clone(&mock);
        let state = make_hub_state_with_upstream(mock).await;
        let msg = WeixinMessage {
            context_token: Some("ctx-err-test".to_string()),
            from_user_id: Some("user-err".to_string()),
            ..Default::default()
        };
        // Even with a non-zero ret from upstream, must not panic.
        handle_hub_command(Arc::clone(&state), msg, HubCommand::Status).await;
        assert_eq!(
            mock_ref.polls_ok(),
            1,
            "send_message must still be called once even when upstream returns error"
        );
    }

    /// M1-7: session list with a routed backend but zero sessions must use the
    /// empty-sessions message (not the non-empty list formatter).
    /// Catches `sessions.is_empty()` match guard → false.
    #[tokio::test]
    async fn session_list_with_backend_but_no_sessions_uses_empty_message() {
        let state = make_hub_state().await;
        let hashed = {
            let mut registry = state.clients.registry.write().await;
            let (_plain, hashed, _) = registry.register("backend-a".into(), None, None);
            hashed
        };
        {
            let mut router = state.routing.router.lock().await;
            router.set_route("user-sess", hashed);
        }

        let result = handle_cmd_session_list(&state, "user-sess", "ctx-sess", None).await;
        assert_eq!(
            result,
            format!(
                "当前后端 `backend-a` {}",
                messages::SESSION_LIST_NO_SESSIONS
            ),
            "empty session list must use empty-sessions template, got: {result:?}"
        );
    }

    /// M1-8: when a matching client exists, session list must use the client name
    /// (not redact_token). Catches `c.vtoken == vtoken` → `!=`.
    #[tokio::test]
    async fn session_list_uses_client_name_not_redacted_token() {
        let state = make_hub_state().await;
        let hashed = {
            let mut registry = state.clients.registry.write().await;
            let (_plain, hashed, _) = registry.register("pretty-name".into(), None, None);
            hashed
        };
        {
            let mut router = state.routing.router.lock().await;
            router.set_route("user-named", hashed);
        }

        let result = handle_cmd_session_list(&state, "user-named", "ctx-named", None).await;
        assert!(
            result.contains("pretty-name"),
            "must display registered client name, got: {result:?}"
        );
        // redact_token shortens hashes; the display name path must win.
        assert!(
            result.starts_with("当前后端 `pretty-name`")
                || result.contains("**后端 `pretty-name` 的 sessions：**"),
            "must use client name in header, got: {result:?}"
        );
    }
}
