# Implementation Plan: MessageQueue Trait Abstraction

**Branch**: `feat/message-queue-trait` | **Date**: 2026-06-05 | **Spec**: `spec.md`  
**Input**: Feature specification from `.specify/feat-message-queue-trait/spec.md`

---

## Summary

Introduce a `MessageQueue` trait in the public `ilink-hub` library crate that abstracts the five queue operations currently performed by the concrete `QueueStore` struct. Rename `QueueStore` to `InMemoryQueue`, give it interior mutability, and implement the trait on it. Update `HubState` to hold `Arc<dyn MessageQueue + Send + Sync>` so any downstream crate (e.g., `ilink-saas`) can inject a custom backend (e.g., Redis). Wire backend selection through the `ILINK_QUEUE_BACKEND` environment variable in `main.rs`. The `InMemoryQueue` remains the default, and all current single-tenant behavior is fully preserved.

---

## Technical Context

**Language/Version**: Rust 2021 edition, stable toolchain (minimum 1.75, current stable ~1.78+)  
**Primary Dependencies**: `tokio` (async runtime), `async-trait` (object-safe async trait), `thiserror` (error types)  
**New Dependency**: `async-trait = "0.1"` — required because Rust stable `async fn in trait` is not yet object-safe; `async-trait` boxes the futures, enabling `Arc<dyn MessageQueue>`  
**Storage**: N/A (this feature is about in-memory queue abstraction; Redis is explicitly out of scope)  
**Testing**: `cargo test` with `tokio::test` for async tests; `tokio-test` for mock notifiers  
**Target Platform**: Linux server (primary), macOS (dev), arm64 + amd64 Docker  
**Project Type**: Rust library crate + binary; single project layout  
**Performance Goals**: No regression; queue push/drain must remain sub-millisecond for in-memory backend  
**Constraints**:
- `async fn in trait` object safety requires `async-trait` crate (see `research.md`)
- No `unwrap()`/`expect()` in any non-test production path
- All `pub` items must have `///` rustdoc
- `HubState::new` signature changes: accepts `Arc<dyn MessageQueue>` as new parameter
**Scale/Scope**: Small focused refactor; ~6 files changed, ~3 files new (tests)

---

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — all ✅.*

| Principle | Status | Notes |
|-----------|--------|-------|
| P1 — Code Quality & Standards | ✅ | All `pub` items get rustdoc; `async-trait` adds one new dependency on stable only; no `unwrap()`/`expect()` in trait or implementations; `thiserror` already present |
| P2 — Testing Philosophy | ✅ | All five trait methods must be covered by unit tests on `InMemoryQueue`; overflow, concurrency, and timeout cases specifically required by SC-005 |
| P3 — Architecture Constraints | ✅ | `async-trait` wraps tokio futures; tokio remains sole runtime; trait-based abstraction is explicitly mandated by constitution ("Swappable components MUST be expressed as Rust traits"); module DAG maintained: `hub::queue` remains a leaf |
| P4 — Security Baseline | ✅ | No new credential exposure; vtokens are opaque string keys; no cross-tenant risk added; the queue trait adds no new logging of sensitive values |
| P5 — Performance Baseline | ✅ | `InMemoryQueue` preserves VecDeque + Notify approach; interior mutex adds one lock per operation (same as current external `Mutex<QueueStore>`); 200-message cap preserved |
| P6 — Operations Baseline | ✅ | `ILINK_QUEUE_BACKEND` follows 12-factor config convention; startup logs backend selection at INFO level; startup fails fast with actionable error on misconfiguration |

*No ⚠️ rows — no Complexity Tracking entries required.*

---

## Project Structure

### Documentation (this feature)

```text
.specify/feat-message-queue-trait/
├── spec.md                    # Feature specification (input)
├── plan.md                    # This file
├── research.md                # Phase 0: async-trait vs native, error type design
├── data-model.md              # Phase 1: entity definitions and state transitions
├── contracts/
│   └── trait_definition.md   # Full trait signature with documented methods
└── quickstart.md              # How to implement a custom backend, run tests
```

### Source Code (affected files)

```text
src/
├── lib.rs                     # ADD: pub use hub::queue::MessageQueue
├── error.rs                   # ADD: QueueError variants (or extend HubError)
├── hub/
│   ├── mod.rs                 # CHANGE: HubState.queues → queue: Arc<dyn MessageQueue>
│   └── queue.rs               # CHANGE: define MessageQueue trait, rename QueueStore → InMemoryQueue
├── server/
│   └── routes.rs              # CHANGE: getupdates, register, metrics to use new trait API
└── main.rs                    # CHANGE: ILINK_QUEUE_BACKEND env var, backend factory

Cargo.toml                     # ADD: async-trait = "0.1"

tests/
└── queue_trait_tests.rs       # NEW: integration tests for all 5 trait methods
```

---

## Phase 0: Research Findings

*See `research.md` for full decision records. Summary of resolved unknowns:*

| Unknown | Decision |
|---------|----------|
| `async fn in trait` (stable) vs `async-trait` crate | **Use `async-trait`** — native `async fn in trait` is not object-safe; object safety is required by FR-002 |
| Error type for trait methods | **Extend `HubError`** with `QueueBackend(String)` variant — reuse existing `thiserror`-derived type, avoid new top-level type |
| Notification abstraction | **`async fn wait_notify(&self, vtoken, timeout_secs) → Result<bool>`** — encapsulates both the notify handle and the timeout; backend-agnostic; simplifies call sites |
| `ensure` on the trait | **Remove `ensure` from trait** — `push` and `wait_notify` both auto-create queue entries on demand; `ensure` becomes an internal detail of `InMemoryQueue` |
| `HubState::new` signature | **Add `queue: Arc<dyn MessageQueue + Send + Sync>` parameter** — constructor injection; `main.rs` creates the concrete type before passing it in |
| Interior mutability strategy | **`Mutex<HashMap<String, ClientQueue>>` inside `InMemoryQueue`** — moves the lock inside the struct, making all trait methods `&self`-compatible |

---

## Phase 1: Design Artifacts

### 1.1 — `MessageQueue` Trait (canonical definition)

*Full signature with documentation in `contracts/trait_definition.md`. Reproduced here for plan completeness.*

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use crate::ilink::types::InboundMessage;
use crate::error::HubError;

/// Abstraction over all queue operations performed on behalf of a registered client.
///
/// Each client is identified by a **vtoken** — an opaque string like `vctx_<uuid>`.
/// Implementations of this trait handle buffering, notification, and lifecycle of
/// per-client message queues, whether in-process or backed by an external store.
///
/// # Object Safety
///
/// This trait is intended for use as `Arc<dyn MessageQueue + Send + Sync>`.
/// All async methods are object-safe via the `async_trait` macro.
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Buffer `msg` in the queue identified by `vtoken`.
    ///
    /// Creates the queue entry if it does not exist.
    /// When the buffer is at capacity (`MAX_QUEUE_SIZE`), the oldest message
    /// is silently dropped before the new one is appended (head-drop policy).
    async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<(), HubError>;

    /// Drain and return all pending messages for `vtoken`.
    ///
    /// Returns an empty `Vec` when no messages are pending.
    /// Does NOT block; returns immediately.
    async fn drain(&self, vtoken: &str) -> Result<Vec<InboundMessage>, HubError>;

    /// Wait until a message is available for `vtoken` or `timeout_secs` elapses.
    ///
    /// Returns `true` if a notification was received (message available),
    /// `false` on timeout. Creates a notifier on demand if none exists.
    /// The caller should call [`drain`] after this returns `true`.
    async fn wait_notify(
        &self,
        vtoken: &str,
        timeout_secs: u64,
    ) -> Result<bool, HubError>;

    /// Return current pending message counts keyed by vtoken.
    ///
    /// Used by the `/metrics` endpoint to expose per-client queue depth.
    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError>;

    /// Remove the queue and its associated notifier for `vtoken`.
    ///
    /// Called when a client disconnects or de-registers. Concurrent pushes
    /// to a removed vtoken MUST NOT panic — they silently create a fresh entry.
    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError>;
}
```

**Object safety**: confirmed via `async-trait` — each method is desugared to a `Box<dyn Future>` return; no generic type parameters; no `Self` in non-`where Self: Sized` positions.

### 1.2 — `InMemoryQueue` (renamed from `QueueStore`)

Key structural change: interior mutability replaces external `Mutex<QueueStore>` in `HubState`.

```rust
/// In-process message queue backed by `VecDeque` and `tokio::sync::Notify`.
///
/// This is the default backend, requiring no external dependencies.
/// Thread-safety is provided by an internal `tokio::sync::Mutex`.
pub struct InMemoryQueue {
    /// Per-vtoken queues; protected by an async-safe mutex.
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
}
```

`ClientQueue` is **unchanged**: `VecDeque<InboundMessage>` + `Arc<Notify>`.

The `push`, `drain`, `wait_notify`, `queue_sizes`, and `remove_client` methods all lock `self.queues` internally. `wait_notify` acquires the lock only long enough to clone the `Arc<Notify>`, then releases the lock before calling `.notified().await` — avoiding a deadlock where push cannot acquire the lock because wait_notify holds it while sleeping.

### 1.3 — `HubState` Changes

```rust
pub struct HubState {
    pub upstream:  Arc<UpstreamClient>,
    pub registry:  RwLock<ClientRegistry>,
    pub queue:     Arc<dyn MessageQueue + Send + Sync>,  // WAS: Mutex<QueueStore>
    pub ctx_map:   Mutex<ContextTokenMap>,
    pub router:    Mutex<Router>,
    pub store:     Arc<Store>,
    pub metrics:   Metrics,
}

impl HubState {
    pub fn new(
        upstream: Arc<UpstreamClient>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue + Send + Sync>,  // NEW parameter
    ) -> Arc<Self> { ... }
}
```

Field renamed from `queues` (plural) to `queue` (singular trait object) for API clarity.

### 1.4 — `QueueBackendConfig` and Startup Validation

New function in `main.rs`:

```rust
/// Reads `ILINK_QUEUE_BACKEND` and constructs the appropriate queue backend.
///
/// # Errors
///
/// Returns an error if the backend name is unrecognized, or if a required
/// companion environment variable (e.g., `ILINK_REDIS_URL` for `redis`) is absent.
async fn build_queue_backend() -> Result<Arc<dyn MessageQueue + Send + Sync>> {
    let backend = std::env::var("ILINK_QUEUE_BACKEND")
        .unwrap_or_else(|_| "memory".to_string());
    match backend.to_lowercase().as_str() {
        "memory" | "" => {
            info!(backend = "memory", "queue backend initialized");
            Ok(Arc::new(InMemoryQueue::new()))
        }
        "redis" => {
            let url = std::env::var("ILINK_REDIS_URL").map_err(|_| {
                anyhow::anyhow!(
                    "ILINK_QUEUE_BACKEND=redis requires ILINK_REDIS_URL \
                     (e.g., redis://localhost:6379)"
                )
            })?;
            // Redis backend is out of scope; fail with a clear forward-looking message
            Err(anyhow::anyhow!(
                "Redis queue backend is not yet implemented in this version. \
                 ILINK_REDIS_URL={url} was provided but no RedisQueue exists. \
                 Remove ILINK_QUEUE_BACKEND or set it to 'memory'."
            ))
        }
        other => Err(anyhow::anyhow!(
            "Unknown ILINK_QUEUE_BACKEND={other:?}. \
             Supported values: 'memory'. (redis: planned, not yet available)"
        )),
    }
}
```

### 1.5 — Call Site Changes Summary

| File | Old pattern | New pattern |
|------|-------------|-------------|
| `hub/mod.rs` dispatch | `state.queues.lock().await; queues.ensure(&vtoken); queues.push(...)` | `state.queue.push(&vtoken, msg).await?` |
| `server/routes.rs` register | `state.queues.lock().await; queues.ensure(&vtoken)` | _(removed — push auto-creates)_ |
| `server/routes.rs` getupdates | `queues.notify_handle()` + timeout loop | `state.queue.wait_notify(&vtoken, poll_secs).await?` |
| `server/routes.rs` metrics | `queues.lock().await; queues.queue_sizes()` | `state.queue.queue_sizes().await?` |
| `main.rs` load_clients_from_db | `queues.lock().await; queues.ensure(&vtoken)` | _(removed)_ |
| `lib.rs` | _(no export)_ | `pub use hub::queue::MessageQueue;` |
| `hub/mod.rs` re-export | `pub use queue::{..., QueueStore}` | Replace `QueueStore` with `InMemoryQueue, MessageQueue` |

### 1.6 — Error Handling Extensions

Extend `HubError` in `src/error.rs`:

```rust
#[derive(Debug, Error)]
pub enum HubError {
    // ... existing variants ...

    /// Queue backend operation failed (e.g., connection error for remote backends).
    #[error("queue backend error: {0}")]
    QueueBackend(String),
}
```

`InMemoryQueue` never produces `QueueBackend` errors in practice (all operations are infallible for in-memory), but the `Result` return type future-proofs the trait for remote backends.

Error handling in `hub/mod.rs` dispatcher: replace the current implicit `queues.ensure` + `queues.push` chain with:

```rust
if let Err(e) = state.queue.push(&vtoken, msg).await {
    error!(error = %e, vtoken = %vtoken, "failed to push message to queue");
    state.metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
}
```

### 1.7 — Tests Plan

New file `tests/queue_trait_tests.rs`:

| Test | Description | Covers |
|------|-------------|--------|
| `test_push_and_drain` | Push 3 messages, drain, verify order and count | FR-003, FR-004, FR-009 |
| `test_drain_empty` | Drain on fresh queue returns `[]` | Edge case |
| `test_overflow_head_drop` | Push 201 messages; drain; verify 200 remain, first is message #2 | FR-009, P5 |
| `test_wait_notify_receives` | Spawn task that pushes after 50ms; main awaits `wait_notify(1s)` → should return `true` | FR-005 |
| `test_wait_notify_timeout` | No push; `wait_notify(1s)` → should return `false` | FR-005 |
| `test_queue_sizes` | Push 2 to vtoken A, 3 to vtoken B; verify `queue_sizes()` map | FR-006 |
| `test_remove_client` | Push, remove_client, drain → returns `[]`; push again after remove → succeeds | FR-007 |
| `test_concurrent_push` | 10 tasks push 10 messages each; drain; verify 100 total or 200 cap | Edge case |
| `test_object_safe` | Compile-time test: `let _: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new())` | FR-002 |
| `test_mock_implementation` | Minimal zero-impl struct satisfies trait; passes to a mock `HubState` | FR-001, SC-002 |

---

## Phase 2: Re-evaluated Constitution Check

After completing design artifacts, all principles re-verified:

| Principle | Re-check Result | Notes |
|-----------|-----------------|-------|
| P1 — Code Quality & Standards | ✅ | Trait + `InMemoryQueue` fully documented; all methods return `Result`; no `unwrap()`; `async-trait` is stable-only |
| P2 — Testing Philosophy | ✅ | 10 tests covering all FR items; overflow, concurrency, timeout, and mock implementation verified |
| P3 — Architecture Constraints | ✅ | Trait injection at `HubState::new` follows constitution mandate; `hub::queue` module remains a DAG leaf; tokio-only runtime |
| P4 — Security Baseline | ✅ | No new sensitive data exposure; vtoken is opaque; no cross-tenant risk from queue abstraction |
| P5 — Performance Baseline | ✅ | `wait_notify` releases lock before awaiting `Notify::notified()`, preventing lock contention during long-poll; VecDeque preserved |
| P6 — Operations Baseline | ✅ | 12-factor config via `ILINK_QUEUE_BACKEND`; INFO-level startup log for backend; fast-fail with actionable messages |

**No new violations introduced in Phase 1.**

---

## Implementation Sequence

The following order minimizes breaking intermediate states:

1. **Add `async-trait` to `Cargo.toml`** — dependency only, no code change
2. **Extend `HubError` in `src/error.rs`** — additive, no breaking change
3. **Rewrite `src/hub/queue.rs`**:
   - Define `MessageQueue` trait
   - Rename `QueueStore` → `InMemoryQueue` with interior `Mutex`
   - Implement `MessageQueue` for `InMemoryQueue`
   - Keep `ClientQueue` and `ContextTokenMap` unchanged
   - Keep `MAX_QUEUE_SIZE = 200`
4. **Update `src/hub/mod.rs`**:
   - Change `HubState.queues: Mutex<QueueStore>` → `queue: Arc<dyn MessageQueue + Send + Sync>`
   - Update `HubState::new` signature
   - Update dispatcher to use `state.queue.push(...).await`
   - Update re-exports
5. **Update `src/server/routes.rs`**:
   - Remove `queues.ensure()` calls
   - Replace `notify_handle` + timeout pattern with `wait_notify`
   - Update `queue_sizes` call to handle `Result`
6. **Update `src/main.rs`**:
   - Add `build_queue_backend()` function
   - Pass `queue` to `HubState::new`
   - Remove `queues.ensure()` from `load_clients_from_db`
7. **Update `src/lib.rs`**:
   - Add `pub use hub::queue::MessageQueue`
8. **Write `tests/queue_trait_tests.rs`**:
   - All 10 test cases
9. **`cargo fmt && cargo clippy && cargo test`** — all must pass

---

## Readiness Confirmation

- [x] All `NEEDS CLARIFICATION` items resolved in `research.md`
- [x] Constitution check passed (Phase 1 re-verified)
- [x] All five gate conditions met (justified tech decisions, compatible dependencies, no security regressions, no performance regressions, 12-factor config)
- [x] Design artifacts generated: `research.md`, `data-model.md`, `contracts/trait_definition.md`, `quickstart.md`
- [x] Implementation sequence defined with no circular dependencies
- [x] Ready for `speckit-tasks` breakdown
