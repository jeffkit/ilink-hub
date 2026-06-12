# Hub Module Issues Implement Log

## Milestone 1: Fix [MEM-01] Broadcast path deep clone of `item_list`

### Decisions

- Changed `WeixinMessage.item_list` type from `Option<Vec<MessageItem>>` to `Option<std::sync::Arc<Vec<MessageItem>>>`.
- Wrap the vector in `Arc::new` in the `build_text_reply` constructor within `src/ilink/types.rs`.
- Update all test helpers that construct a `WeixinMessage` to wrap their `item_list` vector in `Arc::new`. This includes:
  - `src/bridge/mod.rs` (in `dispatcher_tests::make_msg`)
  - `tests/hub_routing_integration.rs` (in `make_user_msg`)
  - `src/hub/quote_route.rs` (in `tests::quote_reply` and `resolve_without_ref_returns_none`)
  - `tests/queue_trait_tests.rs` (in `make_msg`)
  - `src/hub/router.rs` (in `route_uses_default_client_when_no_per_user_route` and `route_broadcast_when_no_default_and_no_route` tests)
- Use `Arc::make_mut` to mutate the `item_list` when needed. This ensures copy-on-write semantics so that the shared vector is cloned only when a client attempts to mutate it. This was implemented in:
  - `append_outbound_origin_footer_to_first_text_item` in `src/hub/outbound_label.rs`
  - The broadcast path in `handle_hub_command` within `src/hub/mod.rs`
- In `collect_quoted_content` (within `src/hub/quote_route.rs`), changed the loop to iterate over `items.iter()` instead of `items` because `IntoIterator` is not implemented automatically for `&Arc<Vec<T>>`.

### Problems

- While compiling, `for item in items` in `src/hub/quote_route.rs` failed because `IntoIterator` is not implemented for `&Arc<Vec<MessageItem>>`. Resolved by changing the loop statement to `for item in items.iter()`.
- Mutating `item_list` directly on mutable references failed because `Arc` contents cannot be modified in place. Resolved by using `std::sync::Arc::make_mut(items)` to achieve safe copy-on-write.

### Outcome

- Verified that cloning `WeixinMessage` in the broadcast loop is now incredibly cheap (only copies the Arc reference).
- All verification commands passed completely:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (147 passed, 0 failed, 1 ignored)
  - `cargo build` (exit 0)
- Created `docs/exec-plans/active/todo-hub-2/reviews/m1/review-request.yaml`.

## Milestone 2: Fix [TO-02] DB queries timeout in `build_hub_ext_for_vctx`

### Decisions

- Wrapped the two asynchronous database queries inside `build_hub_ext_for_vctx` (`store.get_active_session_name` and `store.get_backend_session`) with `tokio::time::timeout(std::time::Duration::from_secs(5), ...)` to prevent indefinite blocking in case of database locks or connection exhaustion.
- Handled the timeout and connection errors gracefully by logging warnings using `tracing::warn` and falling back to robust default values:
  - If `get_active_session_name` fails or times out, it falls back to `"default"`.
  - If `get_backend_session` fails or times out, it falls back to `None`.
- Added a `#[cfg(test)] pub fn pool(&self) -> &sqlx::AnyPool` accessor to `Store` in `src/store/mod.rs` to allow unit tests to obtain the database pool.
- Added two unit tests (`test_build_hub_ext_for_vctx_timeout` and `test_build_hub_ext_for_vctx_timeout_with_session_override`) in `src/hub/mod.rs` that temporarily hold/lock the single connection of an in-memory SQLite database, verifying that both queries trigger the 5-second timeout and fallback gracefully (tokio's virtual time is advanced instantly during tests using `tokio::time::pause()`).

### Problems

- Initially, running `#[tokio::test(start_paused = true)]` caused `Store::connect` to fail immediately with pool timeout errors. This happens because SQLx's connection pool initialization depends on timer-based acquires, which fail when virtual time jumps instantly during test setup. Resolved by running normal `#[tokio::test]` and calling `tokio::time::pause()` only *after* the `Store::connect` operation completes.

### Outcome

- Verified that database query timeouts are handled gracefully without blocking the message routing thread.
- All verification commands passed completely:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (123 passed, 0 failed, 0 ignored)
  - `cargo build` (exit 0)
- Created `docs/exec-plans/active/todo-hub-2/reviews/m2/review-request.yaml`.

## Milestone 3: Fix [S-01] vtoken exposure in debug logs

### Decisions

- Modified the debug logging call in the routing path of `Router::route` within `src/hub/router.rs` to only log the first 8 characters of `vtoken` (`&vtoken[..vtoken.len().min(8)]`).
- Added a robust unit test (`route_redacts_vtoken_in_logs`) in `src/hub/router.rs` that registers a mock subscriber to capture and verify that the logged `vtoken` field matches the redacted 8-character prefix.

### Problems

- Writing the unit test with `tracing::Dispatcher` caused compilation errors since the correct struct name in the `tracing` crate is `tracing::Dispatch`. Resolved by changing it to `tracing::Dispatch::new(sub)`.

### Outcome

- Verified that virtual tokens are successfully redacted in routing debug logs to prevent credential leakage.
- All verification commands passed completely:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (124 passed, 0 failed, 0 ignored/filtered)
  - `cargo build` (exit 0)
- Created `docs/exec-plans/active/todo-hub-2/reviews/m3/review-request.yaml`.

## Milestone 4: Fix [C-01] Broadcast persist fire-and-forget window

### Decisions

- Added two `AtomicU64` fields to the `Metrics` struct in `src/hub/mod.rs`, split per call site so operators can distinguish per-message single-row failures from per-broadcast batch failures:
  - `persist_fire_and_forget_failures_forward: AtomicU64` — incremented by the `ForwardTo` fire-and-forget persist.
  - `persist_fire_and_forget_failures_broadcast: AtomicU64` — incremented by the `Broadcast` fire-and-forget persist.
  Doc-comments explain that a non-zero value means context-token mappings were silently dropped on the dispatch hot-path.
- Changed `HubState::metrics` from `Metrics` (value) to `Arc<Metrics>` so the `tokio::spawn`-ed fire-and-forget persist tasks in `dispatch_message` can `clone()` the handle and increment the counter on error. `Metrics::new()` is wrapped in `Arc::new` at construction. All other call sites (`.metrics.fetch_add(1, ...)`) auto-deref through the `Arc` and need no change.
- Wired the counters into both fire-and-forget persist sites in `src/hub/mod.rs`:
  - The single-row `persist_context_token` spawn in `RoutingDecision::ForwardTo` (around line 289) bumps `persist_fire_and_forget_failures_forward` on error.
  - The batched `persist_context_tokens_batch` spawn in `RoutingDecision::Broadcast` (around line 370) bumps `persist_fire_and_forget_failures_broadcast` on error.
  In both, the failure branch logs the existing `tracing::warn!` and additionally does `metrics.<field>.fetch_add(1, Ordering::Relaxed)`.
- Wired both counters into the Prometheus `/metrics` endpoint (`src/server/routes.rs::metrics`) as `ilink_hub_persist_fire_and_forget_failures_total{path="forward_to"}` and `...{path="broadcast"}`. Operators can now `rate(...) > 0` on either label directly from a scrape — the README's alerting guidance is implementable as-written.
- Added a unit test `persist_fire_and_forget_failure_increments_metric` in `src/hub/mod.rs::tests`. The test holds the only connection of an in-memory SQLite pool (`store.pool().begin()`), then spawns a tokio task that calls `persist_context_tokens_batch` using the same fire-and-forget shape as the broadcast dispatch path. It advances paused virtual time past the pool's acquire timeout to force a failure, then asserts (a) the broadcast counter is `>= 1` and (b) the forward counter is `0` (i.e. the two counters are independent).
- Documented the design trade-off in a new `## Design Trade-offs` section of `README.md`, with a `### Broadcast persist is fire-and-forget` subsection covering:
  - The pro: tail latency on the dispatch hot-path stays at queue-push speed; DB contention cannot stall message delivery.
  - The con: a failed persist silently drops the `real_ctx → vctx` mapping; the next inbound message from the same user may be assigned a new vctx and orphan per-backend sessions.
  - The observability story: both counters (and their Prometheus export names with `path` labels) plus the recommended alert rule (`rate(...) > 0`).
  - The escape hatch for callers who need strict durability: replace the `tokio::spawn` with an awaited write (or wrap it in a retry-with-backoff task and a bounded persistence backlog queue).

### Problems

- First attempt used a non-existent `AtomicU64::clone_handle()` method. Resolved by promoting `HubState::metrics` to `Arc<Metrics>` so the spawned task can `Arc::clone` the whole `Metrics` struct and increment the field through normal method calls.
- The first version of `persist_fire_and_forget_failure_increments_metric` wrapped the spawned task in a `tokio::time::timeout` and then asserted the counter was incremented. With `tokio::time::pause()` active, the pool acquire future was queued behind the held transaction but virtual time never advanced, so the outer timeout elapsing (which would `unwrap` the JoinHandle without it ever being polled to completion) caused the counter to remain at zero. Resolved by removing the outer `timeout` and explicitly calling `tokio::time::sleep(Duration::from_secs(60))` to advance virtual time past sqlx's default pool acquire timeout, then awaiting the JoinHandle so the spawned task actually ran its failure branch.
- Test was first written holding `Store` by value and trying to `store.clone()` for the spawned task. `Store` is not `Clone` (callers always use `Arc<Store>`). Resolved by wrapping the test's `Store` in `Arc::new(...)` at construction, matching the production ownership pattern.

### Outcome

- Verified that broadcast-path fire-and-forget persist failures are now observable in metrics rather than only visible via log scraping. The counter starts at 0 on a fresh `HubState` and is incremented on every failed background persist task across both fire-and-forget sites.
- All verification commands passed completely:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (152 passed, 0 failed, 1 ignored doctest)
  - `cargo build` (exit 0)
  - `grep -q "metrics" src/hub/mod.rs` (exit 0 — plan M4 verification clause)
- Created `docs/exec-plans/active/todo-hub-2/reviews/m4/review-request.yaml`.

## Milestone 5: Fix [A-01] Refactor `HubState` struct

### Decisions

- Split the previously-monolithic `HubState` struct (which exposed ~14 public fields) into three cohesive sub-states plus cross-cutting fields:
  - `IlinkConnState` — owns the iLink upstream `UpstreamClient`, the graceful-shutdown `watch::Receiver<bool>`, the upstream status `Arc<AtomicU8>`, the QR-event broadcast `Sender<QrLoginUiEvent>` + last-ready replay slot, and the relogin-trigger broadcast `Sender<()>`. Anything that mutates only when iLink connects, logs in, or sends a QR-ready event lives here.
  - `RoutingState` — owns the per-message dispatch `Mutex<Router>`, the conversation `RwLock<ContextTokenMap>`, and the `Mutex<QuoteRouteIndex>` used for quote-reply routing. Pure in-memory; no I/O.
  - `ClientState` — owns the registered backend `RwLock<ClientRegistry>`, the in-flight pairing `RwLock<PairingRegistry>`, the per-vtoken `Arc<dyn MessageQueue>`, and the `Arc<PollTracker>` for long-poll concurrency detection.
- `HubState` itself now holds the three sub-states (`ilink`, `routing`, `clients`) plus the cross-cutting `Arc<Store>` and `Arc<Metrics>`. The cross-cutting fields stay on `HubState` because they are touched by code from multiple sub-states (dispatcher writes metrics, store is read by routing helpers, etc.).
- Field paths changed from `state.x` to `state.<sub-state>.x` across the dispatcher, hub-command handler, server routes (`routes.rs`), pairing handlers (`server/pairing.rs`), runtime entry point (`runtime/serve.rs`), and health checker (`hub/health.rs`). Each call site now expresses the smallest sub-state grouping it actually needs.
- Tightened the internal helper signature: `push_to_queue` no longer takes `&HubState`; it takes `&Arc<dyn MessageQueue>` and `&Metrics` directly. Both dispatcher paths wire those in explicitly via `&state.clients.queue` and `&state.metrics`.
- Added five unit tests in `src/hub/mod.rs::tests`:
  - `hub_state_new_populates_all_sub_states` — asserts every sub-state is wired with sensible defaults (upstream reachable, iLink status starts at UNKNOWN, routing/registry empty, metrics at zero).
  - `sub_states_are_independently_usable` — exercises each sub-state through its `state.<sub-state>.x` field path without touching the rest of `HubState` (router set_route round-trip, queue push, upstream poll counters).
  - `sub_state_structs_carry_expected_fields` — compile-time check that touches every documented field of each sub-state, so accidental field removal breaks the test.
  - `hub_state_metrics_are_shared_with_sub_state_paths` — verifies that the `Arc<Metrics>` shared via `state.metrics` is the same instance any spawned task would observe (production pattern).
  - `quote_index_evictor_takes_sub_state_path` — confirms the quote-index evictor's lock is reachable through the new `state.routing.quote_index` path.

### Problems

- First draft of `sub_states_are_independently_usable` called `state.ilink.upstream.send_message(...)` to demonstrate reachability. The production `UpstreamClient::send_message` makes a real HTTP POST against the configured `base_url`, which would have made the test flaky / slow. Resolved by switching the assertion to a counter read (`state.ilink.upstream.polls_ok.load(...)`) which proves the field path is wired without crossing the network boundary.
- The first test-helper `make_state` was synchronous, which left callers having to `.await` the already-built `Arc<HubState>` (yielding "Arc is not a future" errors). Resolved by making `make_state` itself `async fn` so the inner `Store::connect(...)` is awaited inside the helper, and callers receive a ready `Arc<HubState>`.

### Outcome

- Verified that `HubState` is now decomposed into cohesive sub-states. External callers continue to operate against the same `Arc<HubState>` handle; the change is purely structural and does not alter behaviour, lock order, or visibility. Internal helpers express the smallest slice of state they need.
- All verification commands passed completely:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (158 passed: 132 lib + 7 + 9 + 10 integration; 0 failed; 1 ignored doctest)
  - `cargo build` (exit 0)
- Created `docs/exec-plans/active/todo-hub-2/reviews/m5/review-request.yaml`.
