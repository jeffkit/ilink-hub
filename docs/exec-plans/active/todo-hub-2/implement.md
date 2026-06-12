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
