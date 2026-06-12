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
