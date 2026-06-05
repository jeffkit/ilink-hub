---
description: "Task list for MessageQueue trait abstraction"
feature: "feat-message-queue-trait"
generated: "2026-06-05"
spec: "spec.md"
plan: "plan.md"
---

# Tasks: MessageQueue Trait Abstraction

**Branch**: `feat/message-queue-trait`
**Input**: `.specify/feat-message-queue-trait/spec.md`, `.specify/feat-message-queue-trait/plan.md`
**Prerequisites**: plan.md ✅ | spec.md ✅ | research.md ✅ | data-model.md ✅ | contracts/trait_definition.md ✅

## Format: `[ID] [P?] [Story?] Description`

- **[P]**: Can run in parallel (different files, no unmet dependencies)
- **[US1]**: User Story 1 — Zero-Dependency Default (P1)
- **[US2]**: User Story 2 — Trait-Based Extension Point (P2)
- **[US3]**: User Story 3 — Environment-Based Backend Selection (P3)

---

## Phase 1: Setup (Dependency Addition)

**Purpose**: Add the only new runtime dependency required by this feature. No behavior change.

- [X] T001 Add `async-trait = "0.1"` to `[dependencies]` in `Cargo.toml` and run `cargo fetch` to update `Cargo.lock`

**Checkpoint**: `cargo build` still passes with no source changes.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Define the `HubError` extension and the `MessageQueue` trait + `InMemoryQueue` implementation.
All user story phases depend on this phase completing cleanly.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete; the trait signature is the
shared contract that all call-site updates depend upon.

- [X] T002 Add `QueueBackend(String)` variant with `#[error("queue backend error: {0}")]` to `HubError` in `src/error.rs`; add `/// Queue backend operation failed` rustdoc above the variant
- [X] T003 Define `MessageQueue` trait in `src/hub/queue.rs` using `#[async_trait]` with full rustdoc: `push(&self, vtoken, msg) → Result<(), HubError>`, `drain(&self, vtoken) → Result<Vec<InboundMessage>, HubError>`, `wait_notify(&self, vtoken, timeout_secs: u64) → Result<bool, HubError>`, `queue_sizes(&self) → Result<HashMap<String, usize>, HubError>`, `remove_client(&self, vtoken) → Result<(), HubError>`; add `use async_trait::async_trait;` import; add crate-level `/// Object Safety` doc note confirming `Arc<dyn MessageQueue>` usage
- [X] T004 Rename `QueueStore` → `InMemoryQueue` in `src/hub/queue.rs`: replace `pub struct QueueStore { queues: HashMap<String, ClientQueue> }` with `pub struct InMemoryQueue { queues: tokio::sync::Mutex<HashMap<String, ClientQueue>> }`; add `InMemoryQueue::new()` constructor; implement `MessageQueue` for `InMemoryQueue` — `push` acquires lock + calls `queue.push(msg)` + auto-creates entry if absent, `drain` acquires lock + calls `queue.drain()`, `wait_notify` acquires lock only to clone `Arc<Notify>` then drops lock before awaiting `tokio::time::timeout(Duration::from_secs(timeout_secs), notify.notified())`, `queue_sizes` acquires lock + returns `HashMap`, `remove_client` acquires lock + calls `queues.remove(vtoken)`; annotate all five impl methods with `#[async_trait]` attribute from macro expansion; add rustdoc to `InMemoryQueue` struct; keep `ClientQueue`, `ContextTokenMap`, and `MAX_QUEUE_SIZE = 200` unchanged

**Checkpoint**: `cargo build` compiles `src/hub/queue.rs` in isolation (other files will have unresolved references until Phase 3).

---

## Phase 3: User Story 1 — Zero-Dependency Default (Priority: P1) 🎯 MVP

**Goal**: All existing single-tenant behavior preserved. In-memory queue works as default with no
new configuration required. Every call site updated to use the trait API.

**Independent Test**: Start ilink-hub with no `ILINK_QUEUE_BACKEND` env var set; confirm messages
flow from WeChat through the in-memory queue to the long-poll client identically to the pre-trait
version. `cargo test` queue unit tests all pass.

### Implementation for User Story 1

- [X] T005 [US1] Change `HubState.queues: Mutex<QueueStore>` → `queue: Arc<dyn MessageQueue + Send + Sync>` in the struct definition in `src/hub/mod.rs`; update the corresponding `Arc<Self>` construction in `HubState::new` to `queue: queue.clone()` (parameter injection — the `queue` parameter is added in T006)
- [X] T006 [US1] Update `HubState::new(upstream: Arc<UpstreamClient>, store: Arc<Store>)` signature in `src/hub/mod.rs` to `HubState::new(upstream: Arc<UpstreamClient>, store: Arc<Store>, queue: Arc<dyn MessageQueue + Send + Sync>) → Arc<Self>`; add `/// queue backend injected at construction time` rustdoc on the parameter; remove the now-unused `Mutex::new(QueueStore::new())` initialization
- [X] T007 [US1] Update `dispatch_message` `RoutingDecision::ForwardTo` branch in `src/hub/mod.rs`: replace the `let mut queues = state.queues.lock().await; queues.ensure(&vtoken); queues.push(&vtoken, msg);` block with `if let Err(e) = state.queue.push(&vtoken, msg).await { error!(error = %e, vtoken = %vtoken, "failed to push message to queue"); state.metrics.messages_dropped.fetch_add(1, Ordering::Relaxed); }`
- [X] T008 [US1] Update `dispatch_message` `RoutingDecision::Broadcast` branch in `src/hub/mod.rs`: replace the `let mut queues = state.queues.lock().await; queues.ensure(vtoken); queues.push(vtoken, msg_clone);` block inside the `for vtoken in &online` loop with `if let Err(e) = state.queue.push(vtoken, msg_clone).await { error!(error = %e, vtoken = %vtoken, "failed to push broadcast message to queue"); state.metrics.messages_dropped.fetch_add(1, Ordering::Relaxed); }`
- [X] T009 [US1] Update `handle_hub_command` `HubCommand::Broadcast` branch in `src/hub/mod.rs`: replace the `let mut queues = state.queues.lock().await; queues.ensure(vtoken); queues.push(vtoken, m);` block with `if let Err(e) = state.queue.push(vtoken, m).await { error!(error = %e, vtoken = %vtoken, "failed to push hub broadcast message"); }`
- [X] T010 [US1] Update `pub use queue::{...}` re-export line in `src/hub/mod.rs`: remove `QueueStore` from the list; add `InMemoryQueue` and `MessageQueue` in its place so downstream modules can import the new types
- [X] T011 [US1] Remove the `// Ensure queue exists` block (`let mut queues = state.queues.lock().await; queues.ensure(&vtoken);`) from the `register` handler in `src/server/routes.rs`; `push` now auto-creates queue entries on demand so explicit ensure is unnecessary
- [X] T012 [US1] Replace the `notify_handle` + `tokio::time::timeout` long-poll pattern in the `getupdates` handler in `src/server/routes.rs` with `let notified = state.queue.wait_notify(&vtoken, poll_secs as u64).await;` followed by a `match` / `if let Err(e)` guard; replace the subsequent `let mut queues = state.queues.lock().await; queues.drain(&vtoken)` with `state.queue.drain(&vtoken).await.unwrap_or_default()`; remove unused `tokio::time::timeout` import if no longer referenced
- [X] T013 [US1] Replace `let queues = state.queues.lock().await; queues.queue_sizes()` in the `metrics` handler in `src/server/routes.rs` with `let queue_sizes = state.queue.queue_sizes().await.unwrap_or_default();`; remove the now-unused `Mutex` lock acquisition
- [X] T014 [US1] Remove the `queues.ensure(&vtoken)` call from `load_clients_from_db` in `src/main.rs`: delete `let mut queues = state.queues.lock().await;` and the `queues.ensure(&vtoken);` line inside the `for c in clients` loop; the queue will be created on first `push`
- [X] T015 [US1] Add `async fn build_queue_backend() → Result<Arc<dyn MessageQueue + Send + Sync>>` to `src/main.rs` that reads `ILINK_QUEUE_BACKEND` env var, matches `"memory" | ""` → `Arc::new(InMemoryQueue::new())` with `info!(backend = "memory", "queue backend initialized")`, and returns the `Arc`; update `run_server` to call `let queue = build_queue_backend().await?;` before `HubState::new` and pass `queue` as the third argument to `HubState::new(upstream.clone(), store.clone(), queue)`; add `use ilink_hub::hub::InMemoryQueue;` import

**Checkpoint**: `cargo build` succeeds with zero errors. ilink-hub starts normally with no env vars.
Message flow is functionally identical to the pre-trait version.

---

## Phase 4: User Story 2 — Trait-Based Extension Point (Priority: P2)

**Goal**: The `MessageQueue` trait is part of the public library API. Downstream crates can implement
the trait for custom structs (e.g., `RedisQueue`) and inject them into `HubState`.

**Independent Test**: In a downstream crate with `ilink-hub` as a dependency, `use ilink_hub::MessageQueue;`
compiles without error. A minimal zero-impl struct satisfying the trait compiles and can be wrapped in
`Arc<dyn MessageQueue>`. Verified by `test_object_safe` and `test_mock_implementation` tests.

### Implementation for User Story 2

- [X] T016 [P] [US2] Add `pub use hub::queue::MessageQueue;` to `src/lib.rs` so the trait is part of the crate's public surface; verify `pub use hub::HubState;` remains; add `pub use hub::queue::InMemoryQueue;` for downstream crates that want the default backend
- [X] T017 [US2] Add `/// # Downstream Crate Integration` section to `MessageQueue` trait rustdoc in `src/hub/queue.rs` with a minimal code example showing how a downstream crate implements the trait for a custom struct and passes `Arc::new(CustomQueue::new())` to `HubState::new`

**Checkpoint**: `cargo doc --no-deps` generates documentation for `MessageQueue`. `use ilink_hub::MessageQueue` compiles in an external crate.

---

## Phase 5: User Story 3 — Environment-Based Backend Selection (Priority: P3)

**Goal**: `ILINK_QUEUE_BACKEND` selects the queue backend at startup. Unknown values and `redis` without
`ILINK_REDIS_URL` produce actionable startup errors.

**Independent Test**: `ILINK_QUEUE_BACKEND=memory` starts cleanly and logs backend selection.
`ILINK_QUEUE_BACKEND=redis` (without `ILINK_REDIS_URL`) fails with a message identifying the missing
variable. `ILINK_QUEUE_BACKEND=kafka` fails with a message listing supported values.

### Implementation for User Story 3

- [X] T018 [US3] Extend `build_queue_backend()` in `src/main.rs` with two additional `match` arms: `"redis"` → validate `ILINK_REDIS_URL` is present (return `Err(anyhow::anyhow!("ILINK_QUEUE_BACKEND=redis requires ILINK_REDIS_URL (e.g., redis://localhost:6379)"))` when absent; return `Err(anyhow::anyhow!("Redis queue backend is not yet implemented..."))` when present); `other` catch-all → `Err(anyhow::anyhow!("Unknown ILINK_QUEUE_BACKEND={other:?}. Supported values: 'memory'. (redis: planned, not yet available)"))` so operators get an explicit list of valid options

**Checkpoint**: `ILINK_QUEUE_BACKEND=memory` starts and logs `queue backend initialized backend=memory`.
`ILINK_QUEUE_BACKEND=redis` exits with code 1 and a human-readable error. `ILINK_QUEUE_BACKEND=kafka` exits with code 1 listing supported values.

---

## Phase 6: Tests

**Purpose**: Cover all five trait methods and the key behavioral invariants mandated by SC-005.
Tests are organized against `InMemoryQueue` directly and verify object safety and mock-ability.

**⚠️ Create `tests/queue_trait_tests.rs` first, then fill in each test function.**

- [X] T019 Create `tests/queue_trait_tests.rs` with the module preamble: `use std::sync::Arc; use ilink_hub::{hub::queue::{InMemoryQueue}, MessageQueue, ilink::types::InboundMessage};` and a helper `fn make_msg(content: &str) -> InboundMessage` that constructs a minimal `InboundMessage` with the given content string
- [X] T020 [P] [US1] Write `#[tokio::test] async fn test_push_and_drain()` in `tests/queue_trait_tests.rs`: push 3 messages to a fresh `InMemoryQueue` for vtoken `"v1"`, drain, assert length == 3 and order is preserved (FIFO) — covers FR-003, FR-004
- [X] T021 [P] [US1] Write `#[tokio::test] async fn test_drain_empty()` in `tests/queue_trait_tests.rs`: call `drain("v1")` on a fresh queue with no prior push, assert result is `Ok(vec![])` — covers edge case from spec
- [X] T022 [P] [US1] Write `#[tokio::test] async fn test_overflow_head_drop()` in `tests/queue_trait_tests.rs`: push 201 messages (content = `format!("msg_{i}")`), drain, assert result length == 200 and `result[0].content == Some("msg_1".to_string())` (message 0 was head-dropped) — covers FR-009, P5
- [X] T023 [P] [US1] Write `#[tokio::test] async fn test_wait_notify_receives()` in `tests/queue_trait_tests.rs`: wrap `InMemoryQueue` in `Arc`, clone the arc, spawn a task that sleeps 50ms then calls `queue.push("v1", make_msg("hello")).await.unwrap()`, call `queue.wait_notify("v1", 2).await.unwrap()`, assert result is `true` — covers FR-005
- [X] T024 [P] [US1] Write `#[tokio::test] async fn test_wait_notify_timeout()` in `tests/queue_trait_tests.rs`: call `wait_notify("v1", 1)` on a fresh queue with no concurrent push, assert result is `Ok(false)` — covers FR-005 timeout path
- [X] T025 [P] [US1] Write `#[tokio::test] async fn test_queue_sizes()` in `tests/queue_trait_tests.rs`: push 2 messages to `"a"` and 3 to `"b"`, call `queue_sizes()`, assert `sizes["a"] == 2` and `sizes["b"] == 3` — covers FR-006
- [X] T026 [P] [US1] Write `#[tokio::test] async fn test_remove_client()` in `tests/queue_trait_tests.rs`: push 2 messages to `"v1"`, call `remove_client("v1")`, drain `"v1"`, assert result is `Ok(vec![])`; push 1 more message after removal, drain, assert length == 1 (push recreates entry) — covers FR-007
- [X] T027 [P] [US1] Write `#[tokio::test] async fn test_concurrent_push()` in `tests/queue_trait_tests.rs`: wrap `InMemoryQueue::new()` in `Arc`, spawn 10 `tokio::task`s each pushing 10 messages to `"v1"`, `join_all`, drain, assert result length <= 200 (cap) and length > 0 (no data loss below cap) — covers concurrency edge case
- [X] T028 [P] [US2] Write `#[test] fn test_object_safe()` in `tests/queue_trait_tests.rs`: `let _: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());` — this is a compile-time test; if this compiles, the trait is object-safe; covers FR-002
- [X] T029 [P] [US2] Write `#[tokio::test] async fn test_mock_implementation()` in `tests/queue_trait_tests.rs`: define a minimal `struct NoopQueue;` in the test file, `#[async_trait] impl MessageQueue for NoopQueue { ... }` returning `Ok(Default::default())` for all methods, assert `let _: Arc<dyn MessageQueue> = Arc::new(NoopQueue)` compiles and `Arc::new(NoopQueue).push("x", make_msg("y")).await.is_ok()` — covers FR-001, SC-002

**Checkpoint**: `cargo test` runs 10 test cases; all pass. No `unwrap()` panics in production code paths.

---

## Phase 7: Polish & Verification

**Purpose**: Confirm that the full implementation compiles cleanly, passes all lints, and meets the
constitution's CI gate requirements (P1, P2).

- [X] T030 Run `cargo fmt -- --check` across the workspace; fix any formatting issues that `rustfmt` flags in the modified files (`Cargo.toml`, `src/error.rs`, `src/hub/queue.rs`, `src/hub/mod.rs`, `src/server/routes.rs`, `src/main.rs`, `src/lib.rs`, `tests/queue_trait_tests.rs`)
- [X] T031 Run `cargo clippy -- -D warnings` and resolve every warning; pay special attention to: unused import warnings after removing `QueueStore` references, `needless_pass_by_ref` on trait method signatures, and any `async_fn_in_trait` suggestions (should be suppressed by `async-trait`)
- [X] T032 Run `cargo build` (release mode: `cargo build --release`) and confirm zero errors; verify the binary starts with `ILINK_QUEUE_BACKEND=memory` and prints the `queue backend initialized backend=memory` log line
- [X] T033 Run `cargo test -- --test-output immediate` and confirm all 10 tests in `tests/queue_trait_tests.rs` pass; confirm no pre-existing tests regressed

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: No dependencies — start immediately
- **Phase 2 (Foundational)**: Depends on Phase 1 (T001 must complete first for `async-trait` import in T003)
- **Phase 3 (US1)**: Depends on Phase 2 — BLOCKS until T002, T003, T004 are all complete
- **Phase 4 (US2)**: Depends on Phase 3 (trait must be in place at call sites before lib.rs export makes sense)
- **Phase 5 (US3)**: Depends on T015 (build_queue_backend exists before adding more arms to it)
- **Phase 6 (Tests)**: Depends on Phase 3 + Phase 4 (test_mock_implementation needs lib.rs export); T019 (preamble) must precede T020–T029
- **Phase 7 (Polish)**: Depends on all prior phases complete

### Within Phase 2 — Sequential Order

```
T002 (HubError QueueBackend variant)
  → T003 (MessageQueue trait — uses HubError in method signatures)
    → T004 (InMemoryQueue impl — implements the trait)
```

### Within Phase 3 — Sequential Order with Parallel Opportunities

```
T005 → T006 (HubState struct + constructor, same file, sequential)
  → T007 (dispatch ForwardTo branch, same file)
  → T008 (dispatch Broadcast branch, same file)
  → T009 (handle_hub_command Broadcast, same file)
  → T010 (re-exports, same file)
    → T011 (register route, routes.rs)
    → T012 (getupdates route, routes.rs)
    → T013 (metrics route, routes.rs)
    → T014 (load_clients_from_db, main.rs — parallel with T011-T013 ✅ different file)
      → T015 (build_queue_backend + run_server wire-up, main.rs)
```

> **Note**: T011, T012, T013 are in `routes.rs`; T014, T015 are in `main.rs`. Routes.rs and main.rs
> changes can proceed in parallel once T010 is complete.

### Parallel Opportunities Summary

| Parallel Group | Tasks | Condition |
|----------------|-------|-----------|
| Error + Trait setup | T002 and T003 (partially) | T002 can be written while T003 is drafted |
| Routes vs Main changes | T011-T013 and T014 | After T010 completes |
| Test functions | T020-T029 | After T019 creates the file scaffold |
| lib.rs export | T016 | After T010 (re-exports in hub/mod.rs done) |

---

## Parallel Example: Phase 3 Route + Main Updates

Once `HubState` field and constructor changes (T005–T010) are complete:

```
Terminal A (routes.rs):
  Task T011: Remove ensure() from register handler
  Task T012: Update getupdates to use wait_notify
  Task T013: Update metrics to use queue_sizes().await

Terminal B (main.rs — parallel with A):
  Task T014: Remove ensure() from load_clients_from_db
  Task T015: Add build_queue_backend() + wire into run_server
```

## Parallel Example: Phase 6 Tests

```
After T019 (file scaffold) completes, all test functions can be written in parallel:
  Task T020: test_push_and_drain
  Task T021: test_drain_empty
  Task T022: test_overflow_head_drop
  Task T023: test_wait_notify_receives
  Task T024: test_wait_notify_timeout
  Task T025: test_queue_sizes
  Task T026: test_remove_client
  Task T027: test_concurrent_push
  Task T028: test_object_safe
  Task T029: test_mock_implementation
```

---

## Implementation Strategy

### MVP First (User Story 1 — P1 Only)

1. Complete Phase 1: Setup (T001) — `cargo fetch`
2. Complete Phase 2: Foundational (T002–T004) — trait + InMemoryQueue
3. Complete Phase 3: User Story 1 (T005–T015) — all call sites updated
4. **STOP AND VALIDATE**: `cargo build` passes; service starts; manual smoke test of message flow
5. Deploy if needed — zero behavioral change for existing users

### Incremental Delivery

1. Setup + Foundational → trait contract established
2. **US1 complete** → existing behavior fully preserved; test suite green
3. **US2 complete** → `MessageQueue` is public API; downstream crate integration unblocked
4. **US3 complete** → operators can configure backends at startup
5. Tests + Polish → CI gate passes; ready for merge

### Suggested MVP Scope

**T001 → T002 → T003 → T004 → T005 → T006 → T007 → T008 → T009 → T010 → T011 → T012 → T013 → T014 → T015**

This sequence completes User Story 1 (P1) with zero behavioral regression.
US2 and US3 can follow in separate commits.

---

## Task Count Summary

| Phase | Tasks | Scope |
|-------|-------|-------|
| Phase 1: Setup | 1 | T001 |
| Phase 2: Foundational | 3 | T002–T004 |
| Phase 3: US1 (P1) | 11 | T005–T015 |
| Phase 4: US2 (P2) | 2 | T016–T017 |
| Phase 5: US3 (P3) | 1 | T018 |
| Phase 6: Tests | 11 | T019–T029 |
| Phase 7: Polish | 4 | T030–T033 |
| **Total** | **33** | |

**Tests per User Story**: US1 = 8 (T020–T027), US2 = 2 (T028–T029)

---

## Notes

- All `pub` items introduced by this feature MUST carry `///` rustdoc (constitution P1)
- No `unwrap()` or `expect()` in production paths — `wait_notify` must handle `tokio::time::timeout` returning `Elapsed` cleanly
- The `[P]` marker on Phase 6 test tasks reflects that all 10 functions can be written concurrently (different functions in the same file); in practice with a single developer, write them top-to-bottom
- `remove_client` on a vtoken that was never added MUST NOT panic — the `HashMap::remove` on a missing key is a no-op and is safe
- `wait_notify` MUST release the mutex before calling `.notified().await` to prevent a deadlock where `push` cannot acquire the lock while a long-polling caller is sleeping inside `wait_notify`
- Commit after each phase checkpoint for clean rollback points
