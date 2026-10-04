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

/// List the agents the caller is authorized to call over A2A.
///
/// The caller is identified by its hashed vtoken; an unknown caller gets an
/// empty list (fail-closed). Visibility comes from `ILINK_AGENT_ALLOWLIST`:
/// a `*` edge for the caller exposes every registered agent, otherwise only the
/// explicitly granted targets are listed (including their `description` /
/// `persona` metadata).
pub async fn list_agents(state: &Arc<HubState>, caller_vtoken: &str) -> Value {
    let agents: Vec<Value> = {
        let registry = state.clients.registry.read().await;
        let Some(caller) = registry.get_by_vtoken(caller_vtoken) else {
            return agents_content(&[]);
        };
        let allowed = state.a2a_acl.visible_targets_for_agent(&caller.name);
        let mut clients: Vec<_> = registry.all_clients_in(allowed.as_ref());
        clients.sort_by(|a, b| a.name.cmp(&b.name));
        clients
            .iter()
            .map(|c| {
                let mut entry = serde_json::json!({
                    "name": c.name,
                    "online": c.online,
                    "label": c.label,
                });
                if let Some(desc) = &c.description {
                    entry["description"] = serde_json::Value::String(desc.clone());
                }
                entry["persona"] = serde_json::json!({
                    "name": c.persona_name,
                    "emoji": c.persona_emoji
                });
                entry
            })
            .collect()
    };
    agents_content(&agents)
}

fn agents_content(agents: &[Value]) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(agents).unwrap_or_default()
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
    /// Auto-filled by the Hub router from the most-recently updated `active_sessions` row.
    pub vctx: String,
    /// Real WeChat context token (mapped from vctx via the store).
    pub real_ctx: String,
    /// The WeChat peer user id for the conversation.
    pub peer_user_id: String,
    /// Current A2A call-chain depth (0 = direct user message; N = N levels of A2A nesting).
    /// Checked against `MAX_A2A_DEPTH` before proceeding; incremented for the target.
    pub a2a_depth: u8,
}

/// Maximum allowed A2A call-chain depth.  A call at this depth is rejected to
/// prevent runaway recursive agent loops.
pub const MAX_A2A_DEPTH: u8 = 5;

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

    // 2. Caller name (for the notification message and the ACL check).
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

    // 3. A2A allowlist gate (fail-closed: no edge → no call). Rejected before any
    //    side effect: no waiter, no authorization row, no queue push.
    if !state.a2a_acl.allows_a2a(&caller_name, &target_name) {
        return error_content(format!(
            "403: agent '{target_name}' is not authorized for A2A calls from '{caller_name}'"
        ));
    }

    // 4. Register a waiter before pushing the message, so we never miss a fast reply.
    //    The call id doubles as the scope key of this call's authorization row.
    let (call_id, reply_rx) = state.a2a_waiter.register();
    let grant_key = format!("a2a-{call_id}");
    let reply_session = params.session.clone().unwrap_or_else(|| grant_key.clone());

    // 5. Snapshot the target's pre-call state so the one-shot grant can be rolled
    //    back exactly (`release_a2a_grant`).
    let prev_active = match state
        .store
        .get_active_session_row(&ctx.vctx, &target_vtoken)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!(error = %e, target = %target_name, "failed to snapshot target active session");
            None
        }
    };
    let pre_sessions: Option<Vec<String>> = match state
        .store
        .list_backend_sessions(&ctx.vctx, &target_vtoken)
        .await
    {
        Ok(rows) => Some(rows.into_iter().map(|r| r.session_name).collect()),
        Err(e) => {
            // Without a trustworthy snapshot we must not delete anything on release.
            warn!(error = %e, target = %target_name, "failed to snapshot target backend sessions");
            None
        }
    };

    // 6. Persist the target's call-scoped active session with the incremented depth
    //    BEFORE pushing the message — this ensures `get_active_ctx_for_vtoken` on the
    //    target returns the correct depth when the target itself calls `call_agent`.
    let target_depth = ctx.a2a_depth.saturating_add(1);
    if let Err(e) = state
        .store
        .set_active_session_with_depth(&ctx.vctx, &target_vtoken, &grant_key, target_depth)
        .await
    {
        warn!(error = %e, target = %target_name, "failed to persist a2a_depth for target");
    }

    // 7. Push the message into the target Agent's queue.
    //    We construct a synthetic WeixinMessage so the target sees a normal user message.
    let hub_ext = build_hub_ext_for_a2a(
        state,
        &ctx.vctx,
        &target_vtoken,
        &reply_session,
        &call_id,
        target_depth,
    )
    .await;
    let synthetic_msg =
        build_synthetic_message(&ctx.vctx, &ctx.peer_user_id, &params.message, hub_ext);

    crate::hub::push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &target_vtoken,
        synthetic_msg,
    )
    .await;

    // 8. Push the "caller @target: message" notification to WeChat.
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

    // 9. Wait for the target's reply (or timeout).
    let reply = match tokio::time::timeout(CALL_AGENT_TIMEOUT, reply_rx).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => {
            // Sender dropped — target probably went offline.
            state.a2a_waiter.cancel(&call_id);
            release_a2a_grant(
                state,
                &ctx.vctx,
                &target_vtoken,
                &grant_key,
                &prev_active,
                &pre_sessions,
            )
            .await;
            return error_content(format!(
                "Agent '{}' disconnected before replying.",
                target_name
            ));
        }
        Err(_) => {
            // Timeout.
            state.a2a_waiter.cancel(&call_id);
            release_a2a_grant(
                state,
                &ctx.vctx,
                &target_vtoken,
                &grant_key,
                &prev_active,
                &pre_sessions,
            )
            .await;
            return error_content(format!(
                "Agent '{}' did not reply within {} seconds.",
                target_name,
                CALL_AGENT_TIMEOUT.as_secs()
            ));
        }
    };

    // 10. The target's reply (and any `backend_sessions_v2` row it carried) has
    //     landed: the one-shot authorization is no longer needed. The target's
    //     reply was already accepted by `sendmessage` before this point.
    release_a2a_grant(
        state,
        &ctx.vctx,
        &target_vtoken,
        &grant_key,
        &prev_active,
        &pre_sessions,
    )
    .await;

    debug!(
        target = %target_name,
        session = %reply_session,
        "a2a call_agent received reply"
    );

    // 9. Push the reply to WeChat as if spoken by the target (target persona
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

    // 11. Return the reply as MCP tool content, including the session name so
    //    the caller can resume the conversation later.
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": reply
        }],
        "session": reply_session
    })
}

/// Undo everything `call_agent` granted the target for a single call.
///
/// Three steps, all best-effort (authorization cleanup must never turn a
/// delivered reply into an error):
///
/// 1. Drop this call's own row — keyed by `a2a-<call_id>`, so a legitimate
///    grant written by the normal dispatch path for the same pair survives.
/// 2. If the pair now has no row at all, restore the grant that existed before
///    the call (broadcast / `/use` could have legitimately granted it).
/// 3. Reclaim `backend_sessions_v2` by *difference*: every row for this pair
///    that is not in the pre-call snapshot was created during the call (the
///    target's reply echoing a `cli_session_id` under a session name of its
///    choosing), so all of them are dropped — not just the reply session.
///    `backend_sessions_v2` has no TTL, so any survivor would be a permanent
///    authorization via the `backend_sessions_v2` ownership branch.
///
/// `pre_sessions == None` means the pre-call snapshot could not be read: step 3
/// then deletes nothing (fail-safe — never delete a row we cannot prove was
/// created by this call).
async fn release_a2a_grant(
    state: &Arc<HubState>,
    vctx: &str,
    target_vtoken: &str,
    grant_key: &str,
    prev_active: &Option<(String, u8)>,
    pre_sessions: &Option<Vec<String>>,
) {
    if let Err(e) = state
        .store
        .delete_active_session_if_name(vctx, target_vtoken, grant_key)
        .await
    {
        warn!(error = %e, "failed to release a2a grant row");
    }

    match state
        .store
        .get_active_session_row(vctx, target_vtoken)
        .await
    {
        Ok(None) => {
            if let Some((name, depth)) = prev_active {
                if let Err(e) = state
                    .store
                    .set_active_session_with_depth(vctx, target_vtoken, name, *depth)
                    .await
                {
                    warn!(error = %e, "failed to restore pre-a2a active session");
                }
            }
        }
        // A newer row (concurrent dispatch) owns the pair now — leave it alone.
        Ok(Some(_)) => {}
        Err(e) => warn!(error = %e, "failed to inspect active session after a2a release"),
    }

    match pre_sessions {
        None => warn!("skipping a2a backend session reclaim: no trustworthy pre-call snapshot"),
        Some(pre_sessions) => match state.store.list_backend_sessions(vctx, target_vtoken).await {
            Ok(rows) => {
                for row in rows {
                    if pre_sessions.iter().any(|name| name == &row.session_name) {
                        continue;
                    }
                    if let Err(e) = state
                        .store
                        .delete_backend_session(vctx, target_vtoken, &row.session_name)
                        .await
                    {
                        warn!(
                            error = %e,
                            session = %row.session_name,
                            "failed to drop a2a backend session"
                        );
                    }
                }
            }
            Err(e) => warn!(error = %e, "failed to list backend sessions after a2a release"),
        },
    }
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

/// Build `HubExt` for the synthetic A2A message, injecting the call-id and depth so
/// `sendmessage` can resolve the waiter when the target replies and depth is propagated.
async fn build_hub_ext_for_a2a(
    state: &Arc<HubState>,
    vctx: &str,
    target_vtoken: &str,
    session_name: &str,
    call_id: &str,
    a2a_depth: u8,
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
        e.a2a_depth = Some(a2a_depth);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{AdminConfig, HubState, InMemoryQueue};
    use crate::ilink::UpstreamClient;
    use crate::store::Store;
    use std::sync::Arc;

    // ─── persona_handle ──────────────────────────────────────────────────────

    #[test]
    fn persona_handle_with_emoji_and_name() {
        let handle = persona_handle("backend-a", Some("Claude"), Some("🤖"));
        assert_eq!(handle, "🤖 Claude");
    }

    #[test]
    fn persona_handle_name_only_no_emoji() {
        let handle = persona_handle("backend-a", Some("Claude"), None);
        assert_eq!(handle, "Claude");
    }

    #[test]
    fn persona_handle_falls_back_to_backend_name_when_no_persona() {
        let handle = persona_handle("backend-a", None, None);
        assert_eq!(handle, "backend-a");
    }

    #[test]
    fn persona_handle_emoji_without_name_falls_back_to_backend_name() {
        // The match arm `(Some(_emoji), None)` hits `_ => backend_name`
        let handle = persona_handle("backend-a", None, Some("🤖"));
        assert_eq!(handle, "backend-a");
    }

    // ─── build_display_text ──────────────────────────────────────────────────

    #[test]
    fn build_display_text_with_persona_prepends_header() {
        let text = build_display_text("Hello!", "backend-a", Some("Claude"), Some("🤖"));
        assert!(
            text.starts_with("🤖 Claude\n"),
            "must start with persona header: {text:?}"
        );
        assert!(
            text.ends_with("Hello!"),
            "must end with message body: {text:?}"
        );
    }

    #[test]
    fn build_display_text_without_persona_uses_backend_name() {
        let text = build_display_text("Hello!", "backend-a", None, None);
        assert!(text.starts_with("backend-a\n"));
        assert!(text.contains("Hello!"));
    }

    #[test]
    fn build_display_text_empty_body_produces_only_header() {
        let text = build_display_text("", "backend-a", None, None);
        assert_eq!(text, "backend-a\n");
    }

    // ─── build_synthetic_message ─────────────────────────────────────────────

    #[test]
    fn build_synthetic_message_has_correct_fields() {
        let msg = build_synthetic_message("vctx-1", "user-1", "hello agent", None);
        assert_eq!(msg.context_token.as_deref(), Some("vctx-1"));
        assert_eq!(msg.from_user_id.as_deref(), Some("user-1"));
        assert_eq!(msg.message_type, Some(1), "must be text message type");

        let items = msg.item_list.expect("item_list must be present");
        assert_eq!(items.len(), 1);
        let text = items[0]
            .text_item
            .as_ref()
            .expect("text_item must be present");
        assert_eq!(text.text.as_deref(), Some("hello agent"));
    }

    #[test]
    fn build_synthetic_message_with_hub_ext_injects_ext() {
        use crate::ilink::types::HubExt;
        let hub_ext = HubExt {
            session_name: Some("test-session".to_string()),
            session_id: None,
            cli_session_id: None,
            a2a_call_id: Some("call-123".to_string()),
            a2a_depth: Some(1),
            usage: None,
        };
        let msg = build_synthetic_message("vctx-1", "user-1", "hi", Some(hub_ext));
        let ext = msg.ilink_hub_ext.expect("hub_ext must be set");
        assert_eq!(ext.a2a_call_id.as_deref(), Some("call-123"));
        assert_eq!(ext.a2a_depth, Some(1));
        assert_eq!(ext.session_name.as_deref(), Some("test-session"));
    }

    // ─── list_agents integration test ────────────────────────────────────────

    /// Build a state whose `ILINK_AGENT_ALLOWLIST` is `acl_spec`. The env var is
    /// process-wide, so these tests rely on the DB test convention of running
    /// with `--test-threads=1`.
    async fn make_state_with_acl(acl_spec: &str) -> Arc<HubState> {
        let upstream =
            Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("upstream"));
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        temp_env::with_var(crate::hub::ENV_AGENT_ALLOWLIST, Some(acl_spec), || {
            HubState::new(
                upstream,
                store,
                queue,
                shutdown_rx,
                "test-relay-secret".to_string(),
                AdminConfig::from_env(),
            )
        })
    }

    async fn register(state: &Arc<HubState>, name: &str) -> String {
        crate::server::pairing::register_client_in_hub(state, name.to_string(), None, None)
            .await
            .hashed
    }

    fn listed_names(result: &Value) -> Vec<String> {
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        serde_json::from_str::<Vec<serde_json::Value>>(text)
            .expect("agents JSON")
            .iter()
            .filter_map(|a| a["name"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn list_agents_returns_empty_array_when_no_clients() {
        let state = make_state_with_acl("*->*").await;
        let result = list_agents(&state, "unregistered-vtoken").await;

        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .expect("content array");
        assert_eq!(content.len(), 1);
        let text = content[0]
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let agents: serde_json::Value = serde_json::from_str(text).expect("agents JSON");
        assert_eq!(agents, serde_json::json!([]), "no clients → empty array");
    }

    #[tokio::test]
    async fn list_agents_includes_registered_client_fields() {
        let state = make_state_with_acl("caller->test-agent").await;
        let caller = register(&state, "caller").await;
        register(&state, "test-agent").await;

        let result = list_agents(&state, &caller).await;
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .expect("content array");
        let text = content[0]
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let agents: Vec<serde_json::Value> = serde_json::from_str(text).expect("agents JSON");

        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert_eq!(
            agent.get("name").and_then(|v| v.as_str()),
            Some("test-agent")
        );
        assert!(
            agent.get("online").is_some(),
            "online field must be present"
        );
        assert!(
            agent.get("persona").is_some(),
            "persona field must be present"
        );
    }

    #[tokio::test]
    async fn list_agents_returns_clients_sorted_by_name() {
        let state = make_state_with_acl("caller->zebra,caller->alpha,caller->mango").await;
        let caller = register(&state, "caller").await;

        for name in &["zebra", "alpha", "mango"] {
            register(&state, name).await;
        }

        let result = list_agents(&state, &caller).await;
        assert_eq!(
            listed_names(&result),
            vec!["alpha", "mango", "zebra"],
            "must be sorted alphabetically"
        );
    }

    #[tokio::test]
    async fn list_agents_hides_unauthorized_targets() {
        let state = make_state_with_acl("caller->alpha").await;
        let caller = register(&state, "caller").await;
        register(&state, "alpha").await;
        register(&state, "bravo").await;

        let result = list_agents(&state, &caller).await;
        assert_eq!(
            listed_names(&result),
            vec!["alpha"],
            "only allowlisted targets may be listed"
        );
    }

    #[tokio::test]
    async fn list_agents_lists_nothing_without_an_allowlist() {
        let state = make_state_with_acl("").await;
        let caller = register(&state, "caller").await;
        register(&state, "target").await;

        let result = list_agents(&state, &caller).await;
        assert!(
            listed_names(&result).is_empty(),
            "A2A listing must be default-deny"
        );
    }

    #[tokio::test]
    async fn list_agents_includes_description_when_set() {
        let state = make_state_with_acl("caller->described-agent").await;
        let caller = register(&state, "caller").await;
        crate::server::pairing::register_client_in_hub(
            &state,
            "described-agent".to_string(),
            None,
            Some("This agent does cool things".to_string()),
        )
        .await;

        let result = list_agents(&state, &caller).await;
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let agents: Vec<serde_json::Value> = serde_json::from_str(text).expect("JSON");
        let agent = &agents[0];
        assert_eq!(
            agent.get("description").and_then(|v| v.as_str()),
            Some("This agent does cool things")
        );
    }
}
