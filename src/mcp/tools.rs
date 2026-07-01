//! MCP tool implementations: `list_agents` and `call_agent`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

use crate::hub::HubState;
use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};

/// Timeout for waiting for the target Agent's reply.
const CALL_AGENT_TIMEOUT: Duration = Duration::from_secs(120);

// ─── list_agents ─────────────────────────────────────────────────────────────

pub async fn list_agents(state: &Arc<HubState>) -> Value {
    let registry = state.clients.registry.read().await;
    let agents: Vec<Value> = {
        let mut clients: Vec<_> = registry.all_clients().into_iter().collect();
        clients.sort_by(|a, b| a.name.cmp(&b.name));
        clients
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "online": c.online,
                    "label": c.label,
                })
            })
            .collect()
    };
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&agents).unwrap_or_default()
        }]
    })
}

// ─── call_agent ──────────────────────────────────────────────────────────────

pub struct CallAgentParams {
    pub target_name: String,
    pub message: String,
    pub session: Option<String>,
}

pub struct CallAgentContext {
    /// Hashed vtoken of the calling Agent (derived from Bearer header).
    pub caller_vtoken: String,
    /// The WeChat conversation context token the caller is currently serving.
    /// Extracted from `HubExt` that was injected when the caller received the
    /// original user message.  Without this we cannot push messages to WeChat.
    pub vctx: String,
    /// Real WeChat context token (mapped from vctx via the store).
    pub real_ctx: String,
    /// The WeChat peer user id for the conversation.
    pub peer_user_id: String,
}

pub async fn call_agent(
    state: &Arc<HubState>,
    ctx: CallAgentContext,
    params: CallAgentParams,
) -> Value {
    // 1. Resolve target vtoken.
    let (target_vtoken, target_name, target_persona_name, target_persona_emoji) = {
        let registry = state.clients.registry.read().await;
        match registry.get_by_alias(&params.target_name) {
            Some(c) => (
                c.vtoken.clone(),
                c.name.clone(),
                c.persona_name.clone(),
                c.persona_emoji.clone(),
            ),
            None => {
                return error_content(format!(
                    "Agent '{}' not found or not registered.",
                    params.target_name
                ));
            }
        }
    };

    // 2. Caller name (for the notification message).
    let (caller_name, caller_persona_name, caller_persona_emoji) = {
        let registry = state.clients.registry.read().await;
        registry
            .get_by_vtoken(&ctx.caller_vtoken)
            .map(|c| {
                (
                    c.name.clone(),
                    c.persona_name.clone(),
                    c.persona_emoji.clone(),
                )
            })
            .unwrap_or_else(|| ("unknown".to_string(), None, None))
    };

    // 3. Determine session name for the target.
    let session_name = params
        .session
        .clone()
        .unwrap_or_else(|| format!("a2a-{}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f")));

    // 4. Register a waiter before pushing the message, so we never miss a fast reply.
    let (call_id, reply_rx) = state.a2a_waiter.register();

    // 5. Push the message into the target Agent's queue.
    //    We construct a synthetic WeixinMessage so the target sees a normal user message.
    let hub_ext =
        build_hub_ext_for_a2a(state, &ctx.vctx, &target_vtoken, &session_name, &call_id).await;
    let synthetic_msg =
        build_synthetic_message(&ctx.vctx, &ctx.peer_user_id, &params.message, hub_ext);

    crate::hub::push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &target_vtoken,
        synthetic_msg,
    )
    .await;

    // 6. Push the "caller @target: message" notification to WeChat.
    let target_handle = persona_handle(
        &target_name,
        target_persona_name.as_deref(),
        target_persona_emoji.as_deref(),
    );
    let notification_text = format!("@{}\n{}", target_handle, params.message);
    push_wechat_message(
        state,
        &ctx.real_ctx,
        &ctx.peer_user_id,
        &notification_text,
        &caller_name,
        caller_persona_name.as_deref(),
        caller_persona_emoji.as_deref(),
    )
    .await;

    // 7. Wait for the target's reply (or timeout).
    let reply = match tokio::time::timeout(CALL_AGENT_TIMEOUT, reply_rx).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => {
            // Sender dropped — target probably went offline.
            state.a2a_waiter.cancel(&call_id);
            return error_content(format!(
                "Agent '{}' disconnected before replying.",
                target_name
            ));
        }
        Err(_) => {
            // Timeout.
            state.a2a_waiter.cancel(&call_id);
            return error_content(format!(
                "Agent '{}' did not reply within {} seconds.",
                target_name,
                CALL_AGENT_TIMEOUT.as_secs()
            ));
        }
    };

    debug!(
        target = %target_name,
        session = %session_name,
        "a2a call_agent received reply"
    );

    // 8. Push the reply to WeChat as if spoken by the target (target persona
    // header), with the body `@`-mentioning the caller so the user sees which
    // agent the reply is addressed to. The target's own sendmessage is
    // suppressed in the Hub (A2A waiter path) and never reaches WeChat.
    let caller_handle = persona_handle(
        &caller_name,
        caller_persona_name.as_deref(),
        caller_persona_emoji.as_deref(),
    );
    let reply_notification = format!("@{caller_handle}\n{reply}");
    push_wechat_message(
        state,
        &ctx.real_ctx,
        &ctx.peer_user_id,
        &reply_notification,
        &target_name,
        target_persona_name.as_deref(),
        target_persona_emoji.as_deref(),
    )
    .await;

    // 9. Return the reply as MCP tool content, including the session name so
    //    the caller can resume the conversation later.
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": reply
        }],
        "session": session_name
    })
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn error_content(msg: String) -> Value {
    warn!(error = %msg, "call_agent error");
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": msg
        }],
        "isError": true
    })
}

/// Build `HubExt` for the synthetic A2A message, injecting the call-id so
/// `sendmessage` can resolve the waiter when the target replies.
async fn build_hub_ext_for_a2a(
    state: &Arc<HubState>,
    vctx: &str,
    target_vtoken: &str,
    session_name: &str,
    call_id: &str,
) -> Option<HubExt> {
    let mut ext = crate::hub::build_hub_ext_for_vctx(
        &state.store,
        vctx,
        target_vtoken,
        Some(session_name.to_string()),
    )
    .await;
    if let Some(ref mut e) = ext {
        e.a2a_call_id = Some(call_id.to_string());
    }
    ext
}

/// Build a synthetic `WeixinMessage` that looks like a user message to the target.
fn build_synthetic_message(
    vctx: &str,
    peer_user_id: &str,
    text: &str,
    hub_ext: Option<HubExt>,
) -> WeixinMessage {
    use crate::ilink::types::{MessageItem, TextItem};
    use std::sync::Arc as StdArc;

    WeixinMessage {
        context_token: Some(vctx.to_string()),
        from_user_id: Some(peer_user_id.to_string()),
        message_type: Some(1), // text
        item_list: Some(StdArc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            ..Default::default()
        }])),
        ilink_hub_ext: hub_ext,
        ..Default::default()
    }
}

/// Push a text message to the WeChat user on behalf of `sender_name`.
async fn push_wechat_message(
    state: &Arc<HubState>,
    real_ctx: &str,
    to_user_id: &str,
    text: &str,
    sender_name: &str,
    persona_name: Option<&str>,
    persona_emoji: Option<&str>,
) {
    // Build the display text: prepend persona header if available.
    let display_text = build_display_text(text, sender_name, persona_name, persona_emoji);

    let req = SendMessageRequest::reply(real_ctx.to_string(), display_text, to_user_id);
    match state.ilink.upstream.send_message(req).await {
        Ok(resp) if resp.ret.map(|r| r != 0).unwrap_or(false) => {
            warn!(
                ret = resp.ret,
                sender = %sender_name,
                "a2a WeChat notification rejected by upstream"
            );
        }
        Err(e) => {
            warn!(error = %e, sender = %sender_name, "failed to push a2a WeChat notification");
        }
        Ok(_) => {}
    }
}

/// Display handle for an `@`-mention line: persona emoji+name when set, else backend name.
fn persona_handle(
    backend_name: &str,
    persona_name: Option<&str>,
    persona_emoji: Option<&str>,
) -> String {
    match (persona_emoji, persona_name) {
        (Some(emoji), Some(name)) => format!("{} {}", emoji, name),
        (None, Some(name)) => name.to_string(),
        _ => backend_name.to_string(),
    }
}

fn build_display_text(
    text: &str,
    sender_name: &str,
    persona_name: Option<&str>,
    persona_emoji: Option<&str>,
) -> String {
    // Header line: "Emoji PersonaName" or just the raw name if no persona set.
    let header = persona_handle(sender_name, persona_name, persona_emoji);
    format!("{header}\n{text}")
}
