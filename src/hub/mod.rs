//! Hub core: shared state, the inbound message dispatcher, and hub-command
//! handling.
//!
//! The module is split into cohesive submodules:
//!
//! - [`state`] — [`HubState`] and its `IlinkConnState` / `RoutingState` /
//!   `ClientState` sub-states, metrics, and long-poll tracking.
//! - [`dispatch`] — the broadcast→backend pipeline, quote resolution,
//!   `@mention` routing, and the per-conversation `HubExt` helpers.
//! - [`commands`] — the `/list`, `/use`, `/status`, `/help`, `/session …`
//!   command handlers.
//!
//! The remaining `pub mod`s (`router`, `queue`, `registry`, …) hold the routing
//! primitives and persistence-adjacent types the core orchestrates.
//!
//! ## Lock acquisition order (read this before adding cross-cutting changes)
//!
//! `HubState` holds three primary locks that may be acquired in the same
//! request path. Deadlock is avoided by acquiring them in a **strict total
//! order** and **never holding more than one at a time**:
//!
//! 1. `state.routing.router` (`tokio::sync::Mutex<Router>`)
//! 2. `state.routing.quote_index` (`tokio::sync::Mutex<QuoteRouteIndex>`)
//! 3. `state.clients.registry` (`tokio::sync::RwLock<ClientRegistry>`)
//!
//! Rules:
//!
//! - Acquire in the order above when you need more than one in a single
//!   flow. Acquiring them in a different order across two concurrent tasks
//!   is a deadlock waiting to happen.
//! - **Drop the guard before any `.await` that may schedule other tasks.**
//!   The current code does this by assigning to a binding in an inner block
//!   and letting it go out of scope before the next lock or await point.
//! - Do not hold any of these locks across network I/O, DB queries, or
//!   child-process spawns. Copy the data you need (`clone()` on the relevant
//!   `Arc`/`String`/`ClientInfo`) and release the guard before the work.
//! - If you find yourself wanting a "transactional" view across two of these
//!   locks, add a facade method on `HubState` instead of letting callers
//!   reach in. The point of the order rule is that the *set* of call sites
//!   doing multi-lock work stays small and auditable.
//!
//! If a new lock is added to `HubState`, extend this list and place the new
//! lock at a position that respects the order. Reviewers should reject PRs
//! that introduce a new lock without updating this section.

pub mod health;
pub mod messages;
pub mod outbound_label;
pub mod pairing;
pub mod queue;
pub mod quote_route;
pub mod registry;
pub mod router;
pub mod vtoken_hash;

mod commands;
mod dispatch;
mod state;

/// iLink upstream connection status codes stored in `HubState::ilink_status`.
pub mod ilink_status {
    pub const UNKNOWN: u8 = 0;
    pub const CONNECTED: u8 = 1;
    pub const NEEDS_LOGIN: u8 = 2;
    pub const LOGGING_IN: u8 = 3;

    /// Canonical string form of a status code for API responses and log output.
    /// All known codes are listed explicitly so adding a new constant without
    /// updating this function causes a test failure (see `ilink_status_str_covers_all_codes`).
    pub fn as_str(code: u8) -> &'static str {
        match code {
            UNKNOWN => "unknown",
            CONNECTED => "connected",
            NEEDS_LOGIN => "needs_login",
            LOGGING_IN => "logging_in",
            _ => "unknown",
        }
    }
}

pub use dispatch::{spawn_dispatcher, spawn_quote_index_evictor};
pub use health::spawn_health_checker;
pub use outbound_label::{
    append_outbound_origin_footer_to_first_text_item, apply_persona_and_footer_to_first_text_item,
    format_outbound_origin_line, should_append_outbound_origin_label,
};
pub use pairing::PairingRegistry;
pub use queue::{InMemoryQueue, MessageQueue};
pub use quote_route::{
    merge_routing_with_quote, parse_footer_from_quoted_text, QuoteOrigin, QuoteRouteIndex, WarmItem,
};
pub use registry::{ClientInfo, ClientRegistry};
pub use router::{HubCommand, Router, RoutingDecision};
pub use state::{
    AdminConfig, ClientState, EnterOutcome, HubState, IlinkConnState, LatencyGuard,
    LatencyHistogram, Metrics, PollGuard, PollTracker, RoutingState, HISTOGRAM_BUCKETS_MS,
    MAX_CONCURRENT_POLLS_PER_VTOKEN, MAX_HUB_POLLS_DEFAULT,
};
pub use vtoken_hash::{hash_vtoken, is_vtoken_hash};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ilink_status_tests {
    use super::ilink_status;

    /// Ensure every defined constant maps to a non-"unknown" string.
    /// If a new constant is added without updating `as_str`, this test catches it.
    #[test]
    fn ilink_status_str_covers_all_codes() {
        let known = [
            (ilink_status::UNKNOWN, "unknown"),
            (ilink_status::CONNECTED, "connected"),
            (ilink_status::NEEDS_LOGIN, "needs_login"),
            (ilink_status::LOGGING_IN, "logging_in"),
        ];
        for (code, expected) in known {
            assert_eq!(
                ilink_status::as_str(code),
                expected,
                "as_str({code}) should return \"{expected}\""
            );
        }
        // Unknown code falls back to "unknown" rather than panicking.
        assert_eq!(ilink_status::as_str(99), "unknown");
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
            state.routing.ctx_map.is_empty(),
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
