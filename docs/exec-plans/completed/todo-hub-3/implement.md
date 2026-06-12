# Implementation Log - Todo Hub Part 3

## Milestone 1: Fix MISC-01 (LRU Hotness in `resolve_full`)
- **Status**: Completed
- **Changes**:
  - Modified `resolve_full` in `src/hub/queue.rs` to take `&mut self` and use `get` instead of `peek` to correctly update the LRU cache priority.
  - Updated callers in `src/server/routes.rs` to acquire a write lock on `ctx_map` before calling `resolve_full`.
  - Added unit test `test_resolve_full_updates_lru_hotness` in `src/hub/queue.rs` to verify the LRU promotion behavior.
- **Verification**:
  - Run `cargo fmt --check` -> Passed
  - Run `cargo clippy -- -D warnings` -> Passed
  - Run `cargo test` -> Passed
  - Run `cargo build` -> Passed

## Milestone 2: Fix MEM-02 (QuoteRouteIndex Limit)
- **Status**: Completed
- **Changes**:
  - Added a maximum limit (`MAX_BY_CONTENT_KEYS = 10_000`) to `by_content` in `src/hub/quote_route.rs`.
  - Updated `register_outbound_content` to check the current count of keys in the index and skip registration of new keys when the limit is reached, emitting a warning log using `tracing::warn!`.
  - Added unit test `register_outbound_content_respects_limit` in `src/hub/quote_route.rs` to verify that registering entries beyond the limit skips them and logs warnings, while existing keys still update normally.
- **Verification**:
  - Run `cargo fmt --check` -> Passed
  - Run `cargo clippy -- -D warnings` -> Passed
  - Run `cargo test` -> Passed
  - Run `cargo build` -> Passed

## Milestone 3: Fix A-02 (Configurable MAX_QUEUE_SIZE)
- **Status**: Completed
- **Changes**:
  - Updated `build_queue_backend` in `src/runtime/serve.rs` to read the environment variable `ILINK_MAX_QUEUE_SIZE`.
  - Clamped `ILINK_MAX_QUEUE_SIZE` to range `[10, 10_000]` and emitted warning logs if the value was out of bounds or invalid.
  - Refactored `InMemoryQueue` in `src/hub/queue.rs` to support customizable capacity via `InMemoryQueue::with_limit`.
  - Added unit tests `test_in_memory_queue_with_limit` in `src/hub/queue.rs` and `test_build_queue_backend_max_size_clamp` in `src/runtime/serve.rs` to verify clamping, parsing, warning logging, and limit enforcement.
- **Verification**:
  - Run `cargo fmt --check` -> Passed
  - Run `cargo clippy -- -D warnings` -> Passed
  - Run `cargo test` -> Passed
  - Run `cargo build` -> Passed

## Milestone 4: Fix T-02 (Pairing AlreadyConfirmed Test Coverage)
- **Status**: Completed
- **Changes**:
  - Added unit test `double_confirm_returns_already_confirmed` in `src/hub/pairing.rs` that:
    1. Creates a pairing session via `PairingRegistry::create`, marks it scanned via `mark_scanned`.
    2. Calls `confirm` once and asserts success (status `Confirmed`, vtoken set to the supplied value).
    3. Calls `confirm` a second time with the same arguments and asserts the result is `Err(PairingError::AlreadyConfirmed)`.
    4. Verifies the session state remains `Confirmed` and the original vtoken is preserved (so a second confirm does not clobber the first binding).
  - This pins the existing `confirm` contract in `src/hub/pairing.rs:102-125`, which already short-circuits on a second confirm before mutating `vtoken`, `client_name`, or `client_label`. No production code change was required; the T-02 finding was a coverage gap, not a behavior bug.
- **Verification**:
  - Run `cargo fmt --check` -> Passed
  - Run `cargo clippy -- -D warnings` -> Passed
  - Run `cargo test` -> Passed (4 pairing tests including the new `double_confirm_returns_already_confirmed`; 133 lib + 7 breaking_changes + 9 hub_routing_integration + 15 queue_trait_tests all green; 1 doc-test ignored)
  - Run `cargo build` -> Passed
  - Targeted: `cargo test pairing` shows `hub::pairing::tests::double_confirm_returns_already_confirmed ... ok`
