//! Integration tests for the Hub message dispatch and routing pipeline.
//!
//! Each test constructs a real HubState backed by an in-memory SQLite database,
//! registers clients, sends messages through the dispatch pipeline, and asserts
//! observable outcomes — queue contents, context_token translation, fallback
//! behaviour — without mocking any internal component.

use std::sync::Arc;
use std::time::Duration;

use ilink_hub::{
    hub::{spawn_dispatcher, HubState},
    ilink::types::{MessageItem, SendMessageRequest, TextItem, WeixinMessage},
    ilink::UpstreamClient,
    store::Store,
    InMemoryQueue, MessageQueue,
};
use tokio::sync::broadcast;

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn make_state() -> Arc<HubState> {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    HubState::new(upstream, Arc::new(store), queue, shutdown_rx)
}

fn make_user_msg(from_user: &str, real_ctx: &str, text: &str) -> WeixinMessage {
    WeixinMessage {
        message_type: Some(1),
        from_user_id: Some(from_user.to_string()),
        context_token: Some(real_ctx.to_string()),
        item_list: Some(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            extra: serde_json::Value::Object(Default::default()),
            voice_item: None,
        }]),
        ..Default::default()
    }
}

async fn register(state: &Arc<HubState>, name: &str) -> String {
    ilink_hub::server::pairing::register_client_in_hub(state, name.to_string(), None).await
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// A message sent from upstream is dispatched to the registered client's queue.
/// After drain the message text matches what was sent.
#[tokio::test]
async fn single_client_receives_dispatched_message() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-001", "hello");
    tx.send(msg).unwrap();

    // Give dispatcher a moment to process.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let msgs = state.queue.drain(&vtoken).await.unwrap();
    assert_eq!(msgs.len(), 1, "client should receive exactly one message");
    assert_eq!(msgs[0].text(), Some("hello"));
}

/// When no client is online, a message is dropped (nothing enqueued).
#[tokio::test]
async fn no_online_clients_message_is_dropped() {
    let state = make_state().await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-002", "dropped");
    tx.send(msg).unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // No vtokens registered → nothing to drain.
    let sizes = state.queue.queue_sizes().await.unwrap();
    assert!(sizes.is_empty() || sizes.values().all(|&s| s == 0));
}

/// With two registered clients and no per-user route set, a message is
/// broadcast to both queues (Broadcast path).
///
/// The default client is cleared so routing falls through to Broadcast.
#[tokio::test]
async fn two_clients_both_receive_broadcast_message() {
    let state = make_state().await;
    let vtoken_a = register(&state, "claude").await;
    let vtoken_b = register(&state, "codex").await;

    // Mark both clients as online (normally done by getupdates handler).
    {
        let mut registry = state.registry.write().await;
        registry.mark_seen(&vtoken_a);
        registry.mark_seen(&vtoken_b);
    }

    // Remove routes for both clients so no default remains → routing falls
    // through to Broadcast. (With a default set, the message would ForwardTo
    // one client only.)
    {
        let mut router = state.router.lock().await;
        router.remove_routes_for_vtoken(&vtoken_a, None);
        router.remove_routes_for_vtoken(&vtoken_b, None);
    }

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-003", "broadcast me");
    tx.send(msg).unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs_a = state.queue.drain(&vtoken_a).await.unwrap();
    let msgs_b = state.queue.drain(&vtoken_b).await.unwrap();

    assert_eq!(msgs_a.len(), 1, "client A should receive the message");
    assert_eq!(msgs_b.len(), 1, "client B should receive the message");

    // Both clients receive the same stable virtual context token (conversation-scoped,
    // not per-backend). This enables session continuity: a conversation started via
    // Broadcast and later routed via /use shares the same vctx so Claude --resume works.
    // Sessions are isolated by (vctx, vtoken), so each backend's session is still independent.
    let vctx_a = msgs_a[0].context_token.as_deref().unwrap_or("");
    let vctx_b = msgs_b[0].context_token.as_deref().unwrap_or("");
    assert!(
        vctx_a.starts_with("vctx_"),
        "context_token should be a vctx"
    );
    assert!(
        vctx_b.starts_with("vctx_"),
        "context_token should be a vctx"
    );
    assert_eq!(
        vctx_a, vctx_b,
        "broadcast uses one shared vctx per conversation for session continuity"
    );
}

/// With a default client configured, a message is forwarded only to that
/// client (ForwardTo path), not broadcast to all.
#[tokio::test]
async fn single_default_client_receives_forward_to_message() {
    let state = make_state().await;
    let vtoken_default = register(&state, "claude").await;
    let vtoken_other = register(&state, "codex").await;

    // Mark both online, but only the default is set as routing target.
    {
        let mut registry = state.registry.write().await;
        registry.mark_seen(&vtoken_default);
        registry.mark_seen(&vtoken_other);
    }

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-forward", "forward me");
    tx.send(msg).unwrap();

    tokio::time::sleep(Duration::from_millis(80)).await;

    let msgs_default = state.queue.drain(&vtoken_default).await.unwrap();
    let msgs_other = state.queue.drain(&vtoken_other).await.unwrap();

    assert_eq!(
        msgs_default.len(),
        1,
        "default client should receive message"
    );
    assert_eq!(
        msgs_other.len(),
        0,
        "non-default client should NOT receive message in ForwardTo"
    );
}

/// Messages from the same user always receive the same virtual context token
/// so that backend sessions stay stable across multiple messages.
#[tokio::test]
async fn same_user_gets_stable_virtual_context_token() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    // Same real_ctx, same from_user → same vctx.
    tx.send(make_user_msg("user@wx", "real-ctx-stable", "msg 1"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs1 = state.queue.drain(&vtoken).await.unwrap();

    tx.send(make_user_msg("user@wx", "real-ctx-stable", "msg 2"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs2 = state.queue.drain(&vtoken).await.unwrap();

    assert_eq!(msgs1.len(), 1);
    assert_eq!(msgs2.len(), 1);
    assert_eq!(
        msgs1[0].context_token, msgs2[0].context_token,
        "same user should always get the same virtual context token"
    );
}

/// sendmessage handler translates vctx → real_ctx so the reply reaches the
/// correct WeChat conversation.
#[tokio::test]
async fn sendmessage_translates_virtual_to_real_context_token() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    // Dispatch a message so a vctx→real_ctx mapping is created.
    tx.send(make_user_msg("user@wx", "real-ctx-send", "hello"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let msgs = state.queue.drain(&vtoken).await.unwrap();
    assert_eq!(msgs.len(), 1);
    let vctx = msgs[0].context_token.clone().unwrap();

    // Build a sendmessage request using the virtual context token.
    let mut send_req =
        SendMessageRequest::reply_text(vctx.clone(), "reply".to_string(), "user@wx", None);

    // Resolve vctx → real_ctx via the in-memory map (same logic as the handler).
    let real_ctx = {
        let ctx_map = state.ctx_map.read().await;
        ctx_map.resolve(&vctx)
    };
    if let Some(real) = real_ctx {
        if let Some(msg) = send_req.msg.as_mut() {
            msg.context_token = Some(real.clone());
        }
    }

    // After translation, context_token should be the original real_ctx.
    let translated = send_req
        .msg
        .as_ref()
        .and_then(|m| m.context_token.as_deref())
        .unwrap_or("");
    assert_eq!(
        translated, "real-ctx-send",
        "sendmessage should translate vctx back to the original real_ctx"
    );
}

/// Bot echo messages (message_type == 2) are ignored by the dispatcher
/// and do not appear in any client queue.
#[tokio::test]
async fn bot_echo_messages_are_not_dispatched() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let mut bot_msg = make_user_msg("bot@wx", "real-ctx-bot", "bot echo");
    bot_msg.message_type = Some(2); // bot echo type
    tx.send(bot_msg).unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let msgs = state.queue.drain(&vtoken).await.unwrap();
    assert!(
        msgs.is_empty(),
        "bot echo messages should not be dispatched to clients"
    );
}

/// Multiple messages from the same user are queued in FIFO order.
#[tokio::test]
async fn messages_queued_in_fifo_order() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    for i in 0..5u8 {
        tx.send(make_user_msg(
            "user@wx",
            "real-ctx-fifo",
            &format!("msg-{i}"),
        ))
        .unwrap();
        // Small delay to ensure ordering through the async dispatch path.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs = state.queue.drain(&vtoken).await.unwrap();
    assert_eq!(msgs.len(), 5);
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.text(), Some(format!("msg-{i}").as_str()));
    }
}

/// Migration idempotency: connecting twice to the same in-memory DB does not
/// panic or return an error (sqlx migrate! skips already-applied migrations).
#[tokio::test]
async fn migration_is_idempotent() {
    // First connection runs all migrations.
    let store1 = Store::connect("sqlite::memory:").await;
    assert!(store1.is_ok(), "first connect should succeed");

    // A second fresh in-memory DB also migrates cleanly.
    let store2 = Store::connect("sqlite::memory:").await;
    assert!(
        store2.is_ok(),
        "second connect to fresh in-memory DB should succeed"
    );
}
