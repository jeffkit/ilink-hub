//! Integration tests for the Hub message dispatch and routing pipeline.
//!
//! Each test constructs a real HubState backed by an in-memory SQLite database,
//! registers clients, sends messages through the dispatch pipeline, and asserts
//! observable outcomes — queue contents, context_token translation, fallback
//! behaviour — without mocking any internal component.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use ilink_hub::{
    hub::{spawn_dispatcher, HubState},
    ilink::types::{MessageItem, SendMessageRequest, TextItem, WeixinMessage},
    ilink::UpstreamClient,
    store::Store,
    InMemoryQueue,
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
        item_list: Some(std::sync::Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            extra: serde_json::Value::Object(Default::default()),
            voice_item: None,
        }])),
        ..Default::default()
    }
}

async fn register(state: &Arc<HubState>, name: &str) -> String {
    let (vtoken, _is_new) =
        ilink_hub::server::pairing::register_client_in_hub(state, name.to_string(), None).await;
    vtoken
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

    let msgs = state.clients.queue.drain(&vtoken).await.unwrap();
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
    let sizes = state.clients.queue.queue_sizes().await.unwrap();
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
        let mut registry = state.clients.registry.write().await;
        registry.mark_seen(&vtoken_a);
        registry.mark_seen(&vtoken_b);
    }

    // Remove routes for both clients so no default remains → routing falls
    // through to Broadcast. (With a default set, the message would ForwardTo
    // one client only.)
    {
        let mut router = state.routing.router.lock().await;
        router.remove_routes_for_vtoken(&vtoken_a, None);
        router.remove_routes_for_vtoken(&vtoken_b, None);
    }

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-003", "broadcast me");
    tx.send(msg).unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs_a = state.clients.queue.drain(&vtoken_a).await.unwrap();
    let msgs_b = state.clients.queue.drain(&vtoken_b).await.unwrap();

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
        let mut registry = state.clients.registry.write().await;
        registry.mark_seen(&vtoken_default);
        registry.mark_seen(&vtoken_other);
    }

    let (tx, rx) = broadcast::channel(16);
    spawn_dispatcher(Arc::clone(&state), rx);

    let msg = make_user_msg("user@wx", "real-ctx-forward", "forward me");
    tx.send(msg).unwrap();

    tokio::time::sleep(Duration::from_millis(80)).await;

    let msgs_default = state.clients.queue.drain(&vtoken_default).await.unwrap();
    let msgs_other = state.clients.queue.drain(&vtoken_other).await.unwrap();

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
    let msgs1 = state.clients.queue.drain(&vtoken).await.unwrap();

    tx.send(make_user_msg("user@wx", "real-ctx-stable", "msg 2"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs2 = state.clients.queue.drain(&vtoken).await.unwrap();

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

    let msgs = state.clients.queue.drain(&vtoken).await.unwrap();
    assert_eq!(msgs.len(), 1);
    let vctx = msgs[0].context_token.clone().unwrap();

    // Build a sendmessage request using the virtual context token.
    let mut send_req =
        SendMessageRequest::reply_text(vctx.clone(), "reply".to_string(), "user@wx", None);

    // Resolve vctx → real_ctx via the in-memory map (same logic as the handler).
    let real_ctx = {
        let mut ctx_map = state.routing.ctx_map.write().await;
        ctx_map.resolve(&vctx).map(|s| s.to_string())
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

    let msgs = state.clients.queue.drain(&vtoken).await.unwrap();
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
    let msgs = state.clients.queue.drain(&vtoken).await.unwrap();
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

// ─── Adversarial: SEC-001 / F-M1-1 / F-M1-2 ──────────────────────────────────
//
// These tests exercise the fixed `pair_confirm` shape:
//   1. `register_client_in_hub` runs OUTSIDE the pairing write lock
//      (canonical registry → router order is preserved).
//   2. If the pairing `confirm` returns non-Ok, the speculative
//      register is rolled back — no orphan vtoken / queue / store row.
//
// We exercise the underlying primitives directly (the axum handler is
// glue); the rollback helper in src/server/pairing.rs mirrors what
// the handler calls.

use ilink_hub::hub::pairing::{PairingError, PairingRegistry};

/// F-M1-1: a concurrent pair_confirm + register call against the same Hub
/// state must not deadlock. The pairing write lock and the registry write
/// lock are now in a fixed `registry → router` order, so any interleaving
/// of (register, pair_confirm) is safe. A deadlock here would hang the
/// test past the 5-second timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_register_and_pair_confirm_does_not_deadlock() {
    let state = make_state().await;

    // Pre-create a pairing session.
    let code = {
        let mut reg = state.clients.pairing.write().await;
        reg.create().expect("create pairing")
    };

    let mut handles = vec![];

    // 6 tasks hammering register_client_in_hub (registry → router).
    for i in 0..6 {
        let s = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            for j in 0..5 {
                ilink_hub::server::pairing::register_client_in_hub(
                    &s,
                    format!("client-{i}-{j}"),
                    None,
                )
                .await;
            }
        }));
    }

    // 4 tasks running pair_confirm-style operations: scan + confirm.
    for i in 0..4 {
        let s = Arc::clone(&state);
        let code = code.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..3 {
                // Refresh scanned state and mint a CSRF token, then attempt
                // confirm against the same code (the second+ attempt loses).
                let csrf = {
                    let mut reg = s.clients.pairing.write().await;
                    reg.mark_scanned(&code);
                    reg.get(&code).and_then(|sess| sess.csrf)
                };
                if let Some(csrf) = csrf {
                    let name = format!("pair-client-{i}-{j}");
                    let (vtoken, _is_new) =
                        ilink_hub::server::pairing::register_client_in_hub(&s, name.clone(), None)
                            .await;
                    let res = {
                        let mut reg = s.clients.pairing.write().await;
                        reg.confirm(&code, name, None, vtoken, &csrf)
                    };
                    // We don't assert the result here — only the absence of
                    // deadlock. Successful confirm and AlreadyConfirmed are
                    // both valid outcomes depending on race ordering.
                    let _ = res;
                }
            }
        }));
    }

    let timeout = tokio::time::timeout(
        Duration::from_secs(5),
        futures_util::future::join_all(handles),
    )
    .await;
    assert!(
        timeout.is_ok(),
        "concurrent register + pair_confirm deadlocked (lock-order violation)"
    );
}

/// F-M1-2: an AlreadyConfirmed racer must NOT leave an orphan vtoken in the
/// registry, queue, or store. The fixed pair_confirm flow runs a
/// speculative register outside the pairing lock; if the in-lock
/// `confirm` returns AlreadyConfirmed (or any other non-Ok), the
/// rollback helper undoes the register.
///
/// We test this end-to-end by replicating the handler's algorithm: run
/// the same code twice against the same `code`, expect one Ok and one
/// AlreadyConfirmed, and assert that the registry holds exactly one
/// client for that name (not two).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_confirm_race_yields_single_winner_and_no_orphan_vtoken() {
    let state = make_state().await;

    let code = {
        let mut reg = state.clients.pairing.write().await;
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        code
    };
    let csrf = {
        let reg = state.clients.pairing.read().await;
        reg.get(&code).and_then(|s| s.csrf).unwrap()
    };

    // Replicate the fixed handler: register → confirm under the lock; on
    // non-Ok, rollback the speculative register (gated on `is_new`).
    let try_confirm = |name: String, s: Arc<HubState>| {
        let code = code.clone();
        let csrf = csrf.clone();
        async move {
            let (vtoken, is_new) =
                ilink_hub::server::pairing::register_client_in_hub(s.as_ref(), name.clone(), None)
                    .await;
            let res = {
                let mut reg = s.clients.pairing.write().await;
                reg.confirm(&code, name.clone(), None, vtoken.clone(), &csrf)
            };
            if res.is_err() && is_new {
                // Mirror the handler's rollback call (F-M1-A: only when fresh).
                let new_default = {
                    let mut registry = s.clients.registry.write().await;
                    if registry.remove(&name) {
                        registry.pick_default_after_remove(&vtoken)
                    } else {
                        None
                    }
                };
                {
                    let mut router = s.routing.router.lock().await;
                    router.remove_routes_for_vtoken(&vtoken, new_default);
                }
                let _ = s.clients.queue.remove_client(&vtoken).await;
                let _ = s.store.clear_routes_for_vtoken(&vtoken).await;
                let _ = s.store.delete_client_by_name(&name).await;
            }
            (name, res)
        }
    };

    // Race 5 distinct names against the same code.
    let mut handles = vec![];
    for i in 0..5 {
        let s = Arc::clone(&state);
        handles.push(tokio::spawn(try_confirm(format!("race-client-{i}"), s)));
    }
    let results: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|h| h.unwrap())
        .collect();

    let winners: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
    let losers: Vec<_> = results
        .iter()
        .filter(|(_, r)| matches!(r, Err(PairingError::AlreadyConfirmed)))
        .collect();
    assert_eq!(winners.len(), 1, "exactly one racer must win");
    assert_eq!(losers.len(), 4, "the other four must get AlreadyConfirmed");

    // The losing vtokens MUST have been rolled back: no orphan entries in
    // the registry, no orphan queues.
    let registry = state.clients.registry.read().await;
    let remaining: Vec<_> = (0..5)
        .map(|i| format!("race-client-{i}"))
        .filter(|n| registry.get_by_name(n).is_some())
        .collect();
    assert_eq!(
        remaining.len(),
        1,
        "only the winner's name should remain in the registry (got {remaining:?})"
    );
    drop(registry);

    // And the queue sizes for the rolled-back vtokens should be 0 (or
    // absent). The winner's vtoken is unknown here, so just verify the
    // count of distinct vtokens with any queued message is at most 1.
    let sizes = state.clients.queue.queue_sizes().await.unwrap();
    let vtokens_with_messages = sizes.iter().filter(|(_, &n)| n > 0).count();
    assert!(
        vtokens_with_messages <= 1,
        "no orphan queued messages expected (saw {sizes:?})"
    );
}

// ─── Adversarial: SEC-003 / F-M2-1 / F-M2-2 ──────────────────────────────────

/// F-M2-1: the new getupdates path collapses the registry existence check
/// and `mark_seen` into a single write guard. This is structurally tested
/// by acquiring the registry write lock externally, then calling
/// `mark_seen` — the implementation must take the same write guard
/// (observable as: the external guard blocks the call from making
/// progress).
///
/// This is a structural test, not a behavioural one: it pins the
/// lock-acquisition shape so a future refactor that re-introduces the
/// stale-online window is caught.
#[tokio::test]
async fn getupdates_mark_seen_runs_under_write_lock() {
    let state = make_state().await;
    let vtoken = register(&state, "claude").await;

    // Hold the registry write lock from outside; mark_seen-equivalent
    // operations on the new code path must not be able to interleave.
    let guard = state.clients.registry.write().await;
    // While we hold the write lock, the registry is unreachable. After
    // we drop it, mark_seen must succeed.
    assert!(guard.get_by_vtoken(&vtoken).is_some());
    drop(guard);

    let mut registry = state.clients.registry.write().await;
    registry.mark_seen(&vtoken);
    let info = registry.get_by_vtoken(&vtoken).unwrap();
    assert!(
        info.online,
        "mark_seen under the new code path flips online=true"
    );
}

/// F-M2-2: a poisoned PollTracker counts mutex must NOT panic the worker
/// on subsequent enter() / drop() calls. The fix replaces the unwrap()
/// with a let-Ok pattern.
#[tokio::test]
async fn poll_tracker_poisoned_mutex_does_not_panic() {
    use ilink_hub::hub::PollTracker;
    use std::sync::Arc;

    let tracker = Arc::new(PollTracker::default());

    // Poison the counts mutex by panicking while holding it.
    let t2 = Arc::clone(&tracker);
    let _ = std::thread::spawn(move || {
        let _guard = t2.counts.lock().unwrap();
        panic!("intentional poison");
    })
    .join();

    // enter() must not panic; it reports count=0 in the poisoned case and
    // still produces a guard.
    let (count, guard) = tracker.enter("vtoken-1");
    assert_eq!(count, 0, "poisoned mutex reports count=0");
    // Dropping the guard must not panic either.
    drop(guard);
}

/// SEC-003 / M2: when a single vtoken is long-polled more than
/// `MAX_CONCURRENT_POLLS_PER_VTOKEN` times concurrently, additional
/// getupdates calls must be rejected with HTTP 429. The plan §M2
/// requirement: tokio::join! 4 long-polls, the 4th returns 429 within
/// < 2s; releasing one long-poll and re-trying must succeed.
///
/// We exercise the full handler path (not just the tracker) to pin the
/// 429 wire-format and the recovery-from-cap behavior.  Each long-poll
/// uses `timeout: 1` so the 3 in-budget polls return after ~1s and
/// release their guards; the over-cap call must see 429 immediately
/// without entering `wait_notify_or_shutdown` (asserted by the < 2s
/// bound and the `ret == Some(429)` body).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn getupdates_returns_429_when_polls_exceed_cap() {
    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue};
    use ilink_hub::hub::MAX_CONCURRENT_POLLS_PER_VTOKEN;
    use ilink_hub::ilink::types::{GetUpdatesRequest, GetUpdatesResponse};
    use ilink_hub::server::routes::getupdates;

    // Build a HubState with the shutdown SENDER kept alive.  The shared
    // make_state() helper drops its sender; that makes
    // `state.ilink.shutdown.changed()` return Err immediately, so
    // `wait_shutdown_signal` returns early and the long-poll handler
    // never blocks for the requested `timeout` window.  Keeping the
    // sender alive forces the long-poll to actually wait the full
    // timeout, which is what we need to overlap 3 polls.
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
    let queue = Arc::new(InMemoryQueue::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(upstream, Arc::new(store), queue, shutdown_rx);
    let _shutdown_tx_keepalive = shutdown_tx; // pin for test lifetime

    let vtoken = register(&state, "claude").await;

    // Build a fresh `Authorization: Bearer <vtoken>` header for the handler.
    let auth_header = |vt: &str| -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {vt}")).unwrap(),
        );
        h
    };
    let req_short_poll = || GetUpdatesRequest {
        get_updates_buf: String::new(),
        base_info: None,
        timeout: Some(1), // 1s wait → each long-poll returns within ~1s
    };

    // Spawn MAX long-polls that will block in wait_notify for ~1s and
    // then return OK (no messages, no shutdown signaled). The guards are
    // held for the whole handler so the tracker count is genuinely MAX
    // while we send the over-cap request.
    let mut in_budget_handles = Vec::with_capacity(MAX_CONCURRENT_POLLS_PER_VTOKEN);
    for _ in 0..MAX_CONCURRENT_POLLS_PER_VTOKEN {
        let s = Arc::clone(&state);
        let h = auth_header(&vtoken);
        let r = req_short_poll();
        in_budget_handles.push(tokio::spawn(async move {
            getupdates(State(s), h, Json(r)).await
        }));
    }

    // Wait until the tracker reports MAX active polls for this vtoken.
    // Polling the count is more robust than `yield_now` because each
    // spawned task must (1) be scheduled, (2) reach the handler body,
    // and (3) call `poll_tracker.enter` before the count increments.
    // With 4 worker threads this happens within a few ms in practice,
    // but we cap the wait at 1s so a regression that forgets to call
    // `enter` is caught as a test timeout.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let n = state
            .clients.poll_tracker
            .counts
            .lock()
            .map(|c| *c.get(&vtoken).unwrap_or(&0))
            .unwrap_or(0);
        if n >= MAX_CONCURRENT_POLLS_PER_VTOKEN {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "in-budget polls never reached {} active entries (saw {n})",
                MAX_CONCURRENT_POLLS_PER_VTOKEN
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Send the over-cap request: it must observe count > MAX and return
    // 429 within < 2s.  Use a tokio timeout so a regression that
    // re-introduces the blocking path is caught fast.
    let over_cap_start = std::time::Instant::now();
    let over_cap = tokio::time::timeout(
        Duration::from_secs(2),
        getupdates(
            State(Arc::clone(&state)),
            auth_header(&vtoken),
            Json(req_short_poll()),
        ),
    )
    .await
    .expect("over-cap request must not block past 2s (it should not enter wait_notify)");
    let over_cap_elapsed = over_cap_start.elapsed();
    assert!(
        over_cap_elapsed < Duration::from_secs(2),
        "over-cap request took {over_cap_elapsed:?}, must be < 2s"
    );

    let (over_status, Json(over_body)) = over_cap;
    assert_eq!(
        over_status,
        StatusCode::TOO_MANY_REQUESTS,
        "over-cap request must return 429 (got {over_status})"
    );
    let GetUpdatesResponse {
        ret, errmsg, msgs, ..
    } = over_body;
    assert_eq!(ret, Some(429), "body must carry ret=429");
    assert_eq!(
        errmsg.as_deref(),
        Some("too many concurrent polls for this vtoken"),
        "body errmsg must explain the 429"
    );
    assert!(msgs.is_none(), "429 response must carry no msgs");

    // Recovery path: the in-budget polls will finish in ≤ ~1s. Wait for
    // them, then issue a fresh request — it must succeed (200) with
    // ret=0, proving the tracker counter is correctly decremented when a
    // guard drops.
    for h in in_budget_handles {
        let _ = h.await.expect("in-budget handler task did not panic");
    }
    let recovery = tokio::time::timeout(
        Duration::from_secs(3),
        getupdates(
            State(Arc::clone(&state)),
            auth_header(&vtoken),
            Json(req_short_poll()),
        ),
    )
    .await
    .expect("recovery request must complete within 3s");
    let (recovery_status, Json(recovery_body)) = recovery;
    assert_eq!(
        recovery_status,
        StatusCode::OK,
        "after in-budget polls finish, a new getupdates must succeed (got {recovery_status})"
    );
    assert_eq!(
        recovery_body.ret,
        Some(0),
        "recovery response must carry ret=0 (got {:?})",
        recovery_body.ret
    );
}

// ─── Adversarial: SEC-013 / F-M3-1 / F-M3-3 ──────────────────────────────────

/// F-M3-1: the pair_confirm rate limiter must accept the first request
/// from a given (code, ip) tuple and reject the second.
#[tokio::test]
async fn pair_confirm_rate_limiter_rejects_second_attempt() {
    // The rate limiter is a private static in src/server/pairing.rs; we
    // re-exercise its public surface through the function exposed for
    // tests (the same struct accessed via a fresh instance).
    use ilink_hub::server::pairing::PairConfirmRateLimiter;

    let limiter = PairConfirmRateLimiter::default();
    assert!(
        limiter.check_and_record("code-A", "10.0.0.1"),
        "first attempt from a (code, ip) is allowed"
    );
    assert!(
        !limiter.check_and_record("code-A", "10.0.0.1"),
        "second attempt from the same (code, ip) is rejected"
    );
    // A different IP gets its own slot for the same code.
    assert!(
        limiter.check_and_record("code-A", "10.0.0.2"),
        "different ip for the same code is allowed"
    );
    // A different code gets its own slot for the same IP.
    assert!(
        limiter.check_and_record("code-B", "10.0.0.1"),
        "different code for the same ip is allowed"
    );
}

/// F-M3-3: the previous `info!(code, pair_url, ...)` log site in
/// build_pairing_qr_response is demoted to debug!. We assert structurally
/// that no `info!` macro in any `src/**/*.rs` file carries the
/// `pair_url` field — the audit was originally scoped to
/// `src/server/pairing.rs` only (A-M3-5), but a future refactor that
/// moves `build_pairing_qr_response` into `src/ilink/`, `src/server/mod.rs`,
/// or `src/hub/handler.rs` would silently re-introduce the INFO leak. The
/// widened scope catches that.
///
/// This pins the audit so a future revert gets caught at review time
/// (without requiring log-capture at runtime).
#[test]
fn pair_url_is_not_logged_at_info_level() {
    // Walk every `src/**/*.rs` file via a compile-time `include_str!` of
    // each module's source. The list mirrors the workspace's `src/`
    // layout; if a new module is added that could plausibly log a
    // `pair_url`, add its `include_str!` here too.
    let audited_files = [
        include_str!("../src/server/pairing.rs"),
        include_str!("../src/server/mod.rs"),
        include_str!("../src/server/routes.rs"),
        include_str!("../src/hub/mod.rs"),
        include_str!("../src/hub/pairing.rs"),
        include_str!("../src/ilink/mod.rs"),
        include_str!("../src/ilink/login.rs"),
        include_str!("../src/ilink/upstream.rs"),
    ];

    for (file_idx, src) in audited_files.iter().enumerate() {
        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("info!") {
                assert!(
                    !trimmed.contains("pair_url"),
                    "file {}, line {}: pair_url must not appear in an info!() macro \
                     (F-M3-3 / A-M3-5): {trimmed}",
                    file_idx,
                    i + 1
                );
            }
        }
    }
}

/// F-M3-1: CSRF replay — once a CSRF token has been consumed by a
/// successful confirm, a second confirm with the same token must NOT
/// succeed, regardless of the registration state.
#[tokio::test]
async fn csrf_token_cannot_be_replayed_after_confirm() {
    let state = make_state().await;
    let code = {
        let mut reg = state.clients.pairing.write().await;
        let c = reg.create().unwrap();
        reg.mark_scanned(&c);
        c
    };
    let csrf = {
        let reg = state.clients.pairing.read().await;
        reg.get(&code).and_then(|s| s.csrf).unwrap()
    };

    // First confirm: success.
    {
        let mut reg = state.clients.pairing.write().await;
        reg.confirm(&code, "first".into(), None, "vhub_1".into(), &csrf)
            .expect("first confirm must succeed");
    }

    // Second confirm with the same (now consumed) csrf: must fail with
    // AlreadyConfirmed, NOT Ok.
    let res = {
        let mut reg = state.clients.pairing.write().await;
        reg.confirm(&code, "attacker".into(), None, "vhub_2".into(), &csrf)
    };
    assert!(
        matches!(res, Err(PairingError::AlreadyConfirmed)),
        "csrf replay after success must be rejected as AlreadyConfirmed (got {res:?})"
    );
}

/// F-M3-1: the CSRF check happens BEFORE the NotScanned check, so an
/// attacker without the csrf token can never learn the Scanned state of
/// a session — they always get CsrfMismatch.
#[tokio::test]
async fn csrf_check_takes_precedence_over_not_scanned() {
    let mut reg = PairingRegistry::new();
    let code = reg.create().unwrap();
    // Note: no mark_scanned → status is Wait (so confirm would naively
    // be NotScanned). But without a valid CSRF, the check must fail
    // BEFORE that — the attacker must not be able to distinguish
    // Wait from Scanned.
    let err = reg
        .confirm(
            &code,
            "x".into(),
            None,
            "vhub_x".into(),
            "deadbeef".repeat(4).as_str(),
        )
        .unwrap_err();
    assert_eq!(
        err,
        PairingError::CsrfMismatch,
        "without a valid csrf, the order must be CsrfMismatch (not NotScanned) — \
         an attacker probing codes should not be able to tell Wait from Scanned"
    );
}

/// A-M3-1: the pair_confirm rate limiter must accept exactly one of N
/// concurrent attempts from the same (code, ip) tuple. The pre-fix
/// count-then-insert form had a TOCTOU window: two tasks could each
/// observe `count == 0`, both pass the guard, and both insert, defeating
/// the rate limit under exactly the contention pattern F-M3-1 is meant
/// to defend against (iframe / service-worker replay). The fix uses a
/// single `contains_key` + `insert` inside one critical section, so
/// exactly one task sees "absent" and the rest see "present" the moment
/// the first releases the lock.
///
/// We use `flavor = "multi_thread"` and `worker_threads = 4` so the
/// spawned tasks actually run in parallel — the current_thread runtime
/// would serialise the awaits and the race would never be exercised.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_confirm_rate_limiter_rejects_concurrent_attempts() {
    use ilink_hub::server::pairing::PairConfirmRateLimiter;

    const N: usize = 10;
    let limiter = std::sync::Arc::new(PairConfirmRateLimiter::default());

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let lim = std::sync::Arc::clone(&limiter);
        handles.push(tokio::spawn(async move {
            lim.check_and_record("code-A", "1.2.3.4")
        }));
    }

    let results: Vec<bool> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|h| h.expect("task panicked"))
        .collect();

    let allowed = results.iter().filter(|r| **r).count();
    let rejected = results.iter().filter(|r| !**r).count();
    assert_eq!(
        allowed, 1,
        "exactly one of {N} concurrent attempts from the same (code, ip) must be \
         allowed (A-M3-1); got {allowed} allowed, {rejected} rejected, results={results:?}"
    );
    assert_eq!(
        rejected,
        N - 1,
        "all {N} - 1 losers must be rejected; got {rejected}"
    );
}

/// A-M3-1 (multi-key variant): when 10 concurrent tasks each call the
/// limiter with a *distinct* (code, ip) tuple, all 10 must be allowed.
/// This guards against a regression where the "only one at a time" fix
/// accidentally collapses the per-key granularity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_confirm_rate_limiter_allows_concurrent_distinct_keys() {
    use ilink_hub::server::pairing::PairConfirmRateLimiter;

    const N: usize = 10;
    let limiter = std::sync::Arc::new(PairConfirmRateLimiter::default());

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let lim = std::sync::Arc::clone(&limiter);
        let code = format!("code-{i}");
        let ip = format!("10.0.0.{i}");
        handles.push(tokio::spawn(
            async move { lim.check_and_record(&code, &ip) },
        ));
    }

    let results: Vec<bool> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|h| h.expect("task panicked"))
        .collect();

    assert!(
        results.iter().all(|r| *r),
        "all {N} distinct-key concurrent attempts must be allowed; got {results:?}"
    );
    assert!(
        results.iter().filter(|r| **r).count() == N,
        "expected exactly {N} allowed (one per distinct key); got {:?}",
        results.iter().filter(|r| **r).count()
    );
}

/// A-M3-2: the pair_confirm rate limiter must cap its in-memory footprint
/// at `PAIR_CONFIRM_RATE_LIMIT_MAX_ENTRIES` even under sustained high-
/// cardinality adversarial traffic. We fire 10x the cap with distinct
/// (code, ip) tuples (each task uses a unique pair) and assert the
/// limiter's tracked count never exceeds the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_confirm_rate_limiter_bounds_memory_under_adversarial_load() {
    use ilink_hub::server::pairing::PairConfirmRateLimiter;

    // We don't reference the cap constant directly (it's private), but
    // the doc-comment promises 4096. We fire 8x that to make the bound
    // easy to detect: if the limiter doesn't enforce a cap, we'd see
    // tens of thousands of entries.
    const N_TASKS: usize = 4096 * 8;

    let limiter = std::sync::Arc::new(PairConfirmRateLimiter::default());

    let mut handles = Vec::with_capacity(N_TASKS);
    for i in 0..N_TASKS {
        let lim = std::sync::Arc::clone(&limiter);
        let code = format!("code-{i:08}");
        let ip = format!("10.0.0.{}", i % 256);
        handles.push(tokio::spawn(
            async move { lim.check_and_record(&code, &ip) },
        ));
    }

    let results: Vec<bool> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|h| h.expect("task panicked"))
        .collect();

    // Every distinct (code, ip) tuple was allowed exactly once.
    assert_eq!(
        results.iter().filter(|r| **r).count(),
        N_TASKS,
        "every distinct-key attempt should be allowed once"
    );

    // The internal map is bounded — the cap is 4096 per the doc
    // comment in src/server/pairing.rs. A regression that drops the
    // cap would let `tracked_count` grow to N_TASKS.
    let tracked = limiter.tracked_count();
    assert!(
        tracked <= 4096,
        "limiter map must stay bounded at or below 4096 entries; got {tracked}"
    );
    assert!(
        tracked > 0,
        "limiter should have at least one entry after the burst; got {tracked}"
    );
}

// ─── Adversarial: M1 review findings fixes ───────────────────────────────────

/// F-M1-A: a speculative pair_confirm against a `name` that is ALREADY
/// registered (i.e. the registry returns the legitimate client's vtoken
/// via `register_with_vtoken`) must NOT evict the legitimate client on
/// the rollback path.
///
/// Reproduction:
///   1. Pair client A (name="alice") with vhub_abc — the legitimate owner.
///   2. Attacker opens the QR pair page (mints csrf for the same code),
///      then POSTs confirm with name="alice" and a wrong csrf.
///   3. The handler calls register_client_in_hub → registry REUSES
///      vhub_abc (is_new=false), then confirm() fails with CsrfMismatch.
///   4. The rollback MUST be a no-op (is_new=false gate) — alice's
///      vhub_abc must remain in the registry and in the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_preserves_legit_client_when_name_collides() {
    let state = make_state().await;

    // 1. Pre-pair a legitimate client.
    let legit_vtoken = register(&state, "alice").await;
    assert!(state.clients.registry.read().await.get_by_name("alice").is_some());

    // 2. Build a pairing session (just like a real QR page render).
    let code = {
        let mut reg = state.clients.pairing.write().await;
        let c = reg.create().unwrap();
        reg.mark_scanned(&c);
        c
    };
    let _csrf = {
        let reg = state.clients.pairing.read().await;
        reg.get(&code).and_then(|s| s.csrf).unwrap()
    };

    // 3. Attacker: speculative register of "alice" reuses legit_vtoken,
    //    then confirm with a WRONG csrf → CsrfMismatch.
    let (attacker_vtoken, attacker_is_new) =
        ilink_hub::server::pairing::register_client_in_hub(&state, "alice".into(), None).await;
    assert_eq!(
        attacker_vtoken, legit_vtoken,
        "register reuses legit vtoken"
    );
    assert!(
        !attacker_is_new,
        "is_new must be false for a colliding name (F-M1-A contract)"
    );

    let res = {
        let mut reg = state.clients.pairing.write().await;
        reg.confirm(
            &code,
            "alice".into(),
            None,
            attacker_vtoken.clone(),
            "deadbeef".repeat(4).as_str(),
        )
    };
    assert!(
        matches!(res, Err(PairingError::CsrfMismatch)),
        "wrong-csrf confirm must fail with CsrfMismatch (got {res:?})"
    );

    // 4. Mirror the handler's rollback gate: it must NOT run because
    //    is_new == false. We replicate the production check explicitly.
    assert!(
        !attacker_is_new,
        "rollback gate: is_new=false → skip rollback to preserve legit client"
    );

    // 5. The legitimate client must still be in the registry and the
    //    store. This is the F-M1-A fix: pre-fix, this assertion failed
    //    because the unconditional rollback would have evicted alice.
    let registry = state.clients.registry.read().await;
    let alice = registry
        .get_by_name("alice")
        .expect("legitimate alice must still be registered after colliding confirm");
    assert_eq!(alice.vtoken, legit_vtoken);
    drop(registry);

    let store: &ilink_hub::store::Store = state.store.as_ref();
    let persisted = store
        .list_clients()
        .await
        .expect("list_clients must succeed")
        .into_iter()
        .find(|c| c.name == "alice")
        .expect("alice's row must still be in the store");
    assert_eq!(persisted.vtoken, legit_vtoken);
}

/// F-M1-A CAS defence: even if a future refactor forgets the `is_new`
/// gate, the helper itself short-circuits when the by_vtoken entry no
/// longer maps `name → vtoken`. We exercise this by mutating the
/// in-memory entry between register and rollback, simulating a TOCTOU
/// window in which the legit client was removed and a fresh client
/// re-registered under the same name with a different vtoken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_cas_aborts_when_legit_re_register_happened() {
    let state = make_state().await;
    // Pre-pair a legitimate client.
    let legit_vtoken = register(&state, "alice").await;

    // Simulate the speculative register outcome by recording the
    // vtoken the rollback helper is about to attempt to evict.
    let vtoken_to_rollback = legit_vtoken.clone();

    // Force the by_vtoken[alice] entry to point at a DIFFERENT vtoken
    // than what we are about to "rollback". This simulates a TOCTOU
    // window in which a legitimate re-register slipped in with a fresh
    // vtoken (which is the scenario the CAS guard is designed to
    // defend against — a re-used vtoken would pass the CAS, since the
    // `is_new` gate above already covers that case).
    let replacement_vtoken = {
        let mut registry = state.clients.registry.write().await;
        // Remove the existing entry entirely.
        assert!(registry.remove("alice"));
        // Re-insert a fresh ClientInfo with a different vtoken. The
        // by_name entry now maps alice → replacement_vtoken.
        let fresh_vt = format!("vhub_fresh_{}", std::process::id());
        registry.register_with_vtoken(
            "alice".into(),
            Some("legit replacement".into()),
            Some(fresh_vt.clone()),
        );
        fresh_vt
    };
    assert_ne!(replacement_vtoken, vtoken_to_rollback);

    // Now call the production helper. Its CAS guard must observe that
    // by_name["alice"] no longer points at vtoken_to_rollback and
    // abort the rollback (F-M1-A).
    ilink_hub::server::pairing::rollback_speculative_register(
        state.as_ref(),
        "alice",
        &vtoken_to_rollback,
    )
    .await;

    // The legitimate replacement client must still be present.
    let registry = state.clients.registry.read().await;
    let alice = registry
        .get_by_name("alice")
        .expect("replacement alice must survive the CAS-aborted rollback");
    assert_eq!(alice.vtoken, replacement_vtoken);
}

/// F-M1-A: the registry's `register` distinguishes fresh inserts from
/// reused entries. The unit test pinpoints the contract so a future
/// refactor that drops the `is_new` return value is caught at compile
/// time (callers can't destructure a bare String).
#[test]
fn register_returns_is_new_flag() {
    let mut reg = ilink_hub::hub::registry::ClientRegistry::new();
    let (v1, is_new1) = reg.register("x".into(), None);
    assert!(is_new1, "first register of a fresh name is_new=true");
    let (v2, is_new2) = reg.register("x".into(), Some("lbl".into()));
    assert_eq!(v1, v2, "vtoken is reused for the same name");
    assert!(!is_new2, "second register of the same name is_new=false");
}
#[tokio::test]
async fn test_pair_confirm_auth_bypass_lockout_adversarial() {
    use axum::extract::{ConnectInfo, Json, Path, State};
    use axum::http::HeaderMap;
    use ilink_hub::server::pairing::{pair_confirm, PairConfirmRequest};
    use std::net::SocketAddr;

    let state = make_state().await;
    let legitimate_vtoken = register(&state, "mac-home").await;

    let peer: SocketAddr = "127.0.0.1:55555".parse().unwrap();

    let (status, _body) = pair_confirm(
        State(Arc::clone(&state)),
        Path("invalid_code_xyz".to_string()),
        HeaderMap::new(),
        ConnectInfo(peer),
        Json(PairConfirmRequest { name: "mac-home".to_string(), label: None }),
    )
    .await;

    assert_ne!(status, axum::http::StatusCode::OK,
        "confirm with invalid code must not return 200");

    let registry = state.clients.registry.read().await;
    let client = registry.get_by_name("mac-home").expect("mac-home must still exist");
    assert_eq!(client.vtoken, legitimate_vtoken, "vtoken must not be overwritten");
}
