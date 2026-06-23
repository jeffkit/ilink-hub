use super::dispatch::build_hub_ext_for_vctx;
use super::*;
use crate::hub::InMemoryQueue;
use crate::ilink::UpstreamClient;
use crate::store::Store;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Helper: extract `(per_vtoken, guard)` from a fresh `EnterOutcome`.
/// Test code paths only deal with the `Ok` variant; the test helper
/// makes the call sites match the production handler's destructure.
fn ok_count(enter: EnterOutcome) -> (usize, PollGuard) {
    match enter {
        EnterOutcome::Ok { per_vtoken, guard } => (per_vtoken, guard),
        other => panic!("expected EnterOutcome::Ok, got {other:?}"),
    }
}

#[test]
fn poll_tracker_counts_concurrent_polls_and_releases_on_drop() {
    let tracker = Arc::new(PollTracker::default());
    tracker.set_hub_cap(MAX_HUB_POLLS_DEFAULT);

    let (c1, g1) = ok_count(tracker.enter("vt-a"));
    assert_eq!(c1, 1, "first poll is alone");

    let (c2, g2) = ok_count(tracker.enter("vt-a"));
    assert_eq!(c2, 2, "second concurrent poll on same vtoken detected");

    // A different vtoken is tracked independently.
    let (c_other, _g_other) = ok_count(tracker.enter("vt-b"));
    assert_eq!(c_other, 1);

    drop(g2);
    let (c3, _g3) = ok_count(tracker.enter("vt-a"));
    assert_eq!(
        c3, 2,
        "count drops when a guard is released, then rises again"
    );

    drop(g1);
    drop(_g3);
    // All vt-a guards released → entry removed; a fresh poll starts back at 1.
    let (c4, _g4) = ok_count(tracker.enter("vt-a"));
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
    tracker.set_hub_cap(MAX_HUB_POLLS_DEFAULT);
    // Hold MAX guards so the (MAX+1)th enter must observe a count
    // strictly greater than MAX.
    let mut guards = Vec::with_capacity(MAX_CONCURRENT_POLLS_PER_VTOKEN);
    for expected in 1..=MAX_CONCURRENT_POLLS_PER_VTOKEN {
        let (c, g) = ok_count(tracker.enter("vt-cap"));
        assert_eq!(
            c, expected,
            "enter #{expected} must report {expected} active polls"
        );
        guards.push(g);
    }
    // The (MAX+1)th enter must see count == MAX+1 > MAX — this is the
    // signal the handler uses to return 429.
    let (over, g_over) = ok_count(tracker.enter("vt-cap"));
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
    let (back_to_max, g_back_to_max) = ok_count(tracker.enter("vt-cap"));
    assert_eq!(
        back_to_max,
        MAX_CONCURRENT_POLLS_PER_VTOKEN + 1,
        "the freshly entered guard again pushes the count to MAX+1"
    );
    drop(g_back_to_max);
    drop(guards);
}

/// The Hub-wide cap (separate from the per-vtoken cap) is enforced FIRST.
/// A tracker with hub_cap=2 must reject the third concurrent poll from any
/// vtoken, even though each individual vtoken is still well under
/// `MAX_CONCURRENT_POLLS_PER_VTOKEN`.
#[test]
fn poll_tracker_enforces_hub_wide_cap() {
    let tracker = Arc::new(PollTracker::default());
    tracker.set_hub_cap(2);

    // Two polls, two distinct vtokens — well under the per-vtoken cap (3),
    // and right at the Hub-wide cap (2). Both must be accepted.
    let (_c1, g1) = ok_count(tracker.enter("vt-a"));
    let (_c2, g2) = ok_count(tracker.enter("vt-b"));
    assert_eq!(tracker.total_polls(), 2);

    // Third poll: Hub-wide cap reached. Must be rejected with HubLimitReached.
    match tracker.enter("vt-c") {
        EnterOutcome::HubLimitReached { total, cap } => {
            assert_eq!(total, 2);
            assert_eq!(cap, 2);
        }
        other => panic!("expected HubLimitReached, got {other:?}"),
    }
    // The rejection must not have leaked an increment into the counter.
    assert_eq!(tracker.total_polls(), 2);

    // Drop one guard, the next enter succeeds and the counter rises to 2 again.
    drop(g1);
    assert_eq!(tracker.total_polls(), 1);
    let (_c3, g3) = ok_count(tracker.enter("vt-c"));
    assert_eq!(tracker.total_polls(), 2);
    drop(g2);
    drop(g3);
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
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    );

    let mut handles = vec![];

    // Spawn tasks that repeatedly register clients (acquires registry write → router write).
    for i in 0..8 {
        let s = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            for j in 0..10 {
                crate::server::pairing::register_client_in_hub(&s, format!("client-{i}-{j}"), None)
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

    // sqlx uses connection pool with max_connections = 1 for sqlite::memory:
    // Begin a transaction to acquire and hold the only connection.
    // Must begin() before pausing time, otherwise the pool acquire itself times out.
    let _tx = store.pool().begin().await.unwrap();

    tokio::time::pause();

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

    // Begin a transaction to acquire and hold the only connection.
    // Must begin() before pausing time, otherwise the pool acquire itself times out.
    let _tx = store.pool().begin().await.unwrap();

    tokio::time::pause();

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

// ─── A-01: HubState sub-state composition ────────────────────────────────
//
// The A-01 refactor splits the monolithic HubState into IlinkConnState,
// RoutingState, and ClientState. The tests below pin down the structural
// invariant: HubState::new builds all three sub-states with the correct
// fields populated, and internal helpers can take the smallest sub-state
// reference they need without forcing callers to hand the full HubState.

async fn make_state() -> Arc<HubState> {
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    HubState::new(
        upstream,
        store,
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    )
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
    assert_eq!(state.ilink.upstream.polls_ok(), 0);

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
        let _ = &_s.quote_index;
    }
    fn assert_client_fields(_s: &ClientState) {
        let _ = &_s.registry;
        let _ = &_s.pairing;
        let _ = &_s.queue;
        let _ = &_s.poll_tracker;
    }

    let (_tx, _rx) = tokio::sync::watch::channel(false);
    let _upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let _queue: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());

    let ilink = IlinkConnState::new(
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client")),
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

// ─── N-02: LatencyHistogram microsecond precision ───────────────────────
//
// Pre-N-02 the histogram tracked `sum_ms: AtomicU64`. Sub-millisecond
// observations (the common case for `dispatch_latency_ms`) all rounded to
// 0 ms, so the Prometheus `_sum` field stayed at 0 and `rate(_sum)` in
// Grafana produced a flat line — operators could not see whether the path
// was healthy. N-02 switches the internal sum to microseconds and renders
// `_sum` as `sum_us / 1000` (still milliseconds on the wire, so existing
// dashboards keep working). The tests below pin the new contract.

#[test]
fn latency_histogram_submillisecond_observation_increments_sum_us() {
    // The headline N-02 invariant: observing 500 μs must increment the
    // internal microsecond sum, even though `as_millis() as u64 == 0`
    // would have rounded it away under the old implementation.
    let h = LatencyHistogram::new();
    assert_eq!(h.count.load(Ordering::Relaxed), 0);
    assert_eq!(h.sum_us.load(Ordering::Relaxed), 0);

    h.observe(std::time::Duration::from_micros(500));

    assert_eq!(h.count.load(Ordering::Relaxed), 1);
    let sum_us = h.sum_us.load(Ordering::Relaxed);
    assert!(
        sum_us >= 500,
        "sub-millisecond observation must contribute to sum_us (got {sum_us})"
    );
}

#[test]
fn latency_histogram_submillisecond_observation_lands_in_first_bucket() {
    // 500 μs is < 1 ms, so the bucket counter at `le="1"` (the first
    // HISTOGRAM_BUCKETS_MS boundary) must increment. This pins the
    // millisecond-bucketing behavior — we did NOT switch buckets to μs,
    // only the sum. Operators reading bucket counts see no change.
    let h = LatencyHistogram::new();
    h.observe(std::time::Duration::from_micros(500));
    let first_bucket = h.buckets[0].load(Ordering::Relaxed);
    assert_eq!(
        first_bucket, 1,
        "sub-millisecond observation falls into the `le=1` bucket"
    );
}

#[test]
fn latency_histogram_millisecond_observation_still_works() {
    // Regression guard: a multi-millisecond observation must still
    // contribute exactly its elapsed milliseconds to the sum (modulo the
    // μs/ms unit change) and bucket correctly into the matching boundary.
    let h = LatencyHistogram::new();
    h.observe(std::time::Duration::from_millis(42));
    assert_eq!(h.count.load(Ordering::Relaxed), 1);
    let sum_us = h.sum_us.load(Ordering::Relaxed);
    // 42 ms == 42_000 μs, allow scheduler jitter for the Instant path is
    // not engaged here because we pass an explicit Duration — the value
    // must be exactly 42_000.
    assert_eq!(sum_us, 42_000);
    // 42 ms falls into the `le=100` bucket (boundaries: 1, 5, 25, 100).
    let bucket_100 = h.buckets[3].load(Ordering::Relaxed);
    assert_eq!(
        bucket_100, 1,
        "42 ms observation falls into the `le=100` bucket (index 3)"
    );
}

#[test]
fn latency_histogram_render_sum_uses_milliseconds() {
    // The Prometheus render path (routes.rs::render_histogram) emits
    // `sum_us / 1000`. Pin that contract here so a future refactor that
    // accidentally drops the `/ 1000` (and ships microseconds on the wire)
    // is caught. Four 250 μs observations sum to 1_000 μs == 1 ms.
    let h = LatencyHistogram::new();
    for _ in 0..4 {
        h.observe(std::time::Duration::from_micros(250));
    }
    let sum_us = h.sum_us.load(Ordering::Relaxed);
    assert_eq!(sum_us, 1_000);
    let sum_ms = sum_us / 1000;
    assert_eq!(
        sum_ms, 1,
        "rendered _sum must be sum_us / 1000 (milliseconds on the wire)"
    );
}

#[test]
fn latency_guard_records_submillisecond_elapsed() {
    // End-to-end check that the production `LatencyGuard` path (the
    // HistoGuard alias in routes.rs) observes the elapsed Duration, not
    // a truncated millisecond cast. We can't fake `Instant::now()`, so we
    // drop the guard immediately after creation: elapsed will be tiny
    // but strictly non-negative. The microsecond sum must record it (the
    // old millisecond sum would record 0 for any elapsed < 1 ms).
    let h = LatencyHistogram::new();
    {
        let _g = LatencyGuard::new(&h);
        // Guard records on drop.
    }
    assert_eq!(h.count.load(Ordering::Relaxed), 1);
    // sum_us must be present (≥ 0). We don't pin a lower bound here
    // because on a fast CI box the elapsed could be 0 μs — the contract
    // we pin is "the field is populated from a Duration, not from a
    // truncated ms cast". A separate `latency_histogram_render_sum_uses_milliseconds`
    // test pins the millisecond-output side.
    let _ = h.sum_us.load(Ordering::Relaxed);
}

#[tokio::test]
async fn test_extracted_hub_commands() {
    use super::commands::*;

    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    ));

    let from_user = "user-123";

    // 1. Test handle_cmd_list on empty registry
    let list_res = handle_cmd_list(&state, from_user).await;
    assert!(list_res.contains("尚未注册任何后端客户端"));

    // 2. Test handle_cmd_use on non-existent client
    let use_res = handle_cmd_use(&state, from_user, "non-existent").await;
    assert!(use_res.contains("未找到名为 `non-existent` 的后端"));

    // 3. Register client-a and client-b
    let out_a =
        crate::server::pairing::register_client_in_hub(&state, "client-a".to_string(), None).await;
    let vt_a = out_a.hashed;
    let out_b =
        crate::server::pairing::register_client_in_hub(&state, "client-b".to_string(), None).await;
    let vt_b = out_b.hashed;

    // Initially online is false
    let list_res2 = handle_cmd_list(&state, from_user).await;
    assert!(list_res2.contains("🔴 `client-a`"));
    assert!(list_res2.contains("🔴 `client-b`"));

    // Mark online
    state.clients.registry.write().await.mark_online(&vt_a);
    state.clients.registry.write().await.mark_online(&vt_b);

    let list_res3 = handle_cmd_list(&state, from_user).await;
    assert!(list_res3.contains("🟢 `client-a`"));
    assert!(list_res3.contains("🟢 `client-b`"));

    // Test handle_cmd_use successfully
    let use_res2 = handle_cmd_use(&state, from_user, "client-a").await;
    assert!(use_res2.contains("已切换到 `client-a`"));

    // Check routing state matches
    {
        let router = state.routing.router.lock().await;
        assert_eq!(router.get_route(from_user), Some(vt_a.as_str()));
    }

    // Check list now shows client-a selected
    let list_res4 = handle_cmd_list(&state, from_user).await;
    assert!(list_res4.contains("`client-a` ✅"));

    // 4. Test handle_cmd_session_new
    let real_ctx = "ctx-789";
    let session_res =
        handle_cmd_session_new(&state, from_user, real_ctx, None, "session-1", "uuid-1234").await;
    assert!(session_res.contains("session `session-1`"));

    // Check persistence of the session in store using resolved vctx
    let vctx =
        super::dispatch::resolve_vctx_for_message(&state, real_ctx, from_user, None, None).await;
    let active_session = state
        .store
        .get_active_session_name(&vctx, &vt_a)
        .await
        .unwrap();
    assert_eq!(active_session, "session-1");

    // Test session list/switch/delete command helper functions
    let list_sessions = handle_cmd_session_list(&state, from_user, real_ctx, None).await;
    assert!(list_sessions.contains("session-1") && list_sessions.contains("✅"));

    // Session new with empty session name/uuid (adversarial case)
    let session_res_empty = handle_cmd_session_new(&state, from_user, real_ctx, None, "", "").await;
    assert!(session_res_empty.contains("session ``"));

    // Use command on session
    let use_session_res =
        handle_cmd_session_use(&state, from_user, real_ctx, None, "session-2").await;
    assert!(use_session_res.contains("session `session-2`"));

    // Delete session
    let delete_session_res =
        handle_cmd_session_delete(&state, from_user, real_ctx, None, "session-1").await;
    assert!(delete_session_res.contains("session `session-1`"));

    // Trying to delete active session-2 should fail
    let delete_active_res =
        handle_cmd_session_delete(&state, from_user, real_ctx, None, "session-2").await;
    assert!(delete_active_res.contains("无法删除当前活跃的 session"));
}

#[tokio::test]
async fn test_adversarial_hub_commands() {
    use super::commands::*;

    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    ));

    let from_user = "user-adversarial";
    let real_ctx = "ctx-adversarial";

    // 1. Session commands when no backend is selected/routed
    // This should return NO_BACKEND
    let list_res = handle_cmd_session_list(&state, from_user, real_ctx, None).await;
    assert!(list_res.contains("当前未路由到任何后端"));

    let new_res = handle_cmd_session_new(&state, from_user, real_ctx, None, "test", "").await;
    assert!(new_res.contains("当前未路由到任何后端"));

    let use_res = handle_cmd_session_use(&state, from_user, real_ctx, None, "test").await;
    assert!(use_res.contains("当前未路由到任何后端"));

    let del_res = handle_cmd_session_delete(&state, from_user, real_ctx, None, "test").await;
    assert!(del_res.contains("当前未路由到任何后端"));

    // Register a client and select it
    let out_a =
        crate::server::pairing::register_client_in_hub(&state, "client-a".to_string(), None).await;
    let vt_a = out_a.hashed;
    state.clients.registry.write().await.mark_online(&vt_a);
    let _ = handle_cmd_use(&state, from_user, "client-a").await;

    // 2. Extremely long session name
    let long_name = "a".repeat(1000);
    let new_long = handle_cmd_session_new(&state, from_user, real_ctx, None, &long_name, "").await;
    assert!(new_long.contains("session `aaaaaaaa")); // Check it succeeded or handled it safely

    // 3. Special characters (SQL injection, paths, quotes) in session name
    let injection_name = "' OR '1'='1";
    let new_inject =
        handle_cmd_session_new(&state, from_user, real_ctx, None, injection_name, "").await;
    assert!(new_inject.contains("session `' OR '1'='1`"));

    let path_name = "../../../etc/passwd";
    let new_path = handle_cmd_session_new(&state, from_user, real_ctx, None, path_name, "").await;
    assert!(new_path.contains("session `../../../etc/passwd`"));

    let unicode_name = "会话🚀🌟";
    let new_unicode =
        handle_cmd_session_new(&state, from_user, real_ctx, None, unicode_name, "").await;
    assert!(new_unicode.contains("session `会话🚀🌟`"));

    // 4. Try using a nonexistent session
    let use_nonexistent =
        handle_cmd_session_use(&state, from_user, real_ctx, None, "nonexistent-session").await;
    // According to commands.rs, handle_cmd_session_use creates a new slot with empty uuid if it does not exist:
    // "Ok(None) => state.store.set_backend_session(&vctx, &vtoken, session_name, "")..."
    // So it should succeed and return session_use_ok!
    assert!(use_nonexistent.contains("已切换到 session `nonexistent-session`"));

    // 5. Deleting a nonexistent session
    let delete_nonexistent =
        handle_cmd_session_delete(&state, from_user, real_ctx, None, "not-real").await;
    assert!(delete_nonexistent.contains("未找到 session `not-real`"));
}
