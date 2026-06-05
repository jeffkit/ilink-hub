# Research: MessageQueue Trait Abstraction

**Feature**: `feat/message-queue-trait`  
**Date**: 2026-06-05  
**Purpose**: Resolve technical unknowns before Phase 1 design.

---

## Decision: Use `async-trait` crate for object-safe async trait methods

**Rationale**: Rust 1.75 (December 2023) stabilized `async fn in trait` (RPIT in traits), but
this feature is **not object-safe** on stable Rust. A trait with `async fn` methods cannot be
used as `dyn Trait` without the experimental `dyn*` feature (nightly-only). The constitution
(P1) mandates stable toolchain exclusively and forbids nightly features. Therefore, to satisfy
FR-002 (object safety for `Arc<dyn MessageQueue>`), we must use the `async-trait` crate.

**Mechanism**: `async-trait` desugars each `async fn` in a trait into:
```rust
fn method(&self, ...) -> Pin<Box<dyn Future<Output = ...> + Send + '_>>;
```
This is heap-allocating (one `Box` per call) but is negligible for queue operations that are
already doing `HashMap` lookups and `VecDeque` mutations.

**Alternatives considered**:
1. *Native `async fn in trait` (stable)*: Rejected — not object-safe; cannot use `dyn MessageQueue`.
2. *Manual desugaring with `Pin<Box<dyn Future>>`*: Equivalent to `async-trait` but far more
   boilerplate; `async-trait` generates identical code. Rejected on ergonomics grounds.
3. *Redesign trait to avoid `async` methods entirely*: E.g., synchronous methods with `Arc<Mutex>`
   internally. Rejected — synchronous locking on an async runtime is forbidden (P3). Using
   `tokio::sync::Mutex` requires `async` at the call site.
4. *Return channel handles instead of async methods*: E.g., `push` sends on an `mpsc` channel
   rather than being async. Rejected — introduces accidental complexity and loses the simple
   request-response error propagation needed for remote backends (FR-012).

**Trade-offs**:
- `async-trait` adds a `Box` heap allocation per trait method invocation
- For `InMemoryQueue`, the operations are sub-microsecond without the box, and still well within
  performance goals with it (p95 < 200ms; queue ops are < 1µs even with boxing)
- `async-trait` is widely used, well-maintained, and is the official community solution until
  native object-safe async traits land on stable Rust

**Dependency**: `async-trait = "0.1"` (adds to `[dependencies]` in `Cargo.toml`)

---

## Decision: Extend `HubError` with `QueueBackend` variant (not a new error type)

**Rationale**: The project already uses `thiserror`-derived `HubError` for all library-facing
errors (P1: "Prefer `thiserror`-derived types for library-facing errors"). Creating a separate
`QueueError` type would require either a new public type with its own `From` impl, or a nested
`HubError::Queue(QueueError)` which adds an extra layer without benefit at this scale.

**Decision**: Add one new variant:
```rust
#[error("queue backend error: {0}")]
QueueBackend(String),
```

This is sufficient for remote backend failures. `InMemoryQueue` operations never produce this
variant in practice (all infallible for in-process state), but the `Result` return type ensures
downstream callers handle errors properly and keeps the trait future-proof for Redis/other backends.

**Alternatives considered**:
1. *New `QueueError` type*: Rejected — unnecessary indirection; would require updating `HubError`
   to add a `Queue(#[from] QueueError)` variant anyway.
2. *Use `anyhow::Error` in trait methods*: Rejected — `anyhow` is only permitted in binary entry
   points (P1). The `MessageQueue` trait is part of the library crate.
3. *Infallible trait (no `Result`)*: Rejected — remote backends can fail; an infallible trait
   would require `panic!` on failure or silent data loss, both forbidden.

**Trade-offs**: Adding a variant to `HubError` is a semver-minor change to the library. Since
the project is at `0.1.0`, this is acceptable without a version bump.

---

## Decision: `wait_notify` encapsulates both notification and timeout (not a handle getter)

**Rationale**: The current API exposes `notify_handle(&self, vtoken) -> Option<Arc<Notify>>`.
This is tokio-specific — a Redis backend would use pub/sub, not `tokio::Notify`. Making the trait
return `Arc<Notify>` would leak an implementation detail and make the trait non-extensible.

**Decision**: Replace with an async method:
```rust
async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError>;
```
Returns `true` if notified (message available), `false` on timeout. Each implementation handles
its own notification primitive internally.

**`InMemoryQueue` implementation strategy**:
1. Lock `self.queues` briefly to clone the `Arc<Notify>` for `vtoken`
2. Release lock immediately (critical: do NOT hold lock while awaiting)
3. `tokio::time::timeout(duration, notify.notified()).await`
4. Return `true` if notified, `false` on timeout

This is equivalent to the current pattern in `routes.rs` but encapsulated inside the trait impl.

**Alternatives considered**:
1. *Return `Arc<dyn Notifier>` (custom trait)*: Would allow heterogeneous notification handles.
   Rejected — over-engineered for current scope; adds a second trait; Redis impl would still need
   to create a tokio task listening on pub/sub and bridging to a `Notify` anyway.
2. *Keep `notify_handle` returning `Arc<Notify>`*: Rejected — not backend-agnostic; exposes
   tokio internals in the public API; would break Redis backend design.
3. *Callback/channel approach*: Rejected — unnecessary complexity; callers still need to await.

**Trade-offs**: The timeout is now an implementation concern (trait impl must call `tokio::time::timeout`), which is a minor coupling to tokio — acceptable given P3 mandates tokio exclusively.

---

## Decision: Remove `ensure` from the public trait; absorb into `push` and `wait_notify`

**Rationale**: `ensure(vtoken)` is a defensive initialization call used in three places:
1. On client registration (in `routes.rs`)
2. On startup DB reload (in `main.rs` `load_clients_from_db`)
3. Implicitly before any dispatch in the dispatcher

The only purpose of `ensure` is to pre-create a `ClientQueue` entry so that a later `wait_notify`
call finds an existing `Arc<Notify>` to clone. If `push` and `wait_notify` both create the entry
on demand, `ensure` is redundant.

**Decision**: Both `push` and `wait_notify` create the queue entry if it doesn't exist. The
`ensure` method is removed from the trait and from all call sites. The `InMemoryQueue` helper
`ensure_entry(&mut guard, vtoken)` remains as a private function.

**Edge case handled**: If `wait_notify` is called for a vtoken that has never been pushed to,
it creates a fresh `ClientQueue`, waits on its `Notify`, and returns `false` on timeout — no panic.

**Alternatives considered**:
1. *Keep `ensure` on the trait*: Creates a second initialization path that callers must remember
   to call. Rejected — implicit initialization in `push`/`wait_notify` is simpler and less
   error-prone.
2. *Remove `ensure` but make push/wait_notify require prior registration*: Rejected — would require
   stricter ordering guarantees that don't add value.

---

## Decision: Interior mutability in `InMemoryQueue` (move lock inside struct)

**Rationale**: The trait methods take `&self` (shared reference) since they must be usable on
`Arc<dyn MessageQueue>`. Mutating the internal HashMap requires interior mutability.

**Decision**: Use `tokio::sync::Mutex<HashMap<String, ClientQueue>>` as the internal field.

```rust
pub struct InMemoryQueue {
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
}
```

`tokio::sync::Mutex` is appropriate here (not `std::sync::Mutex`) because:
- The lock is held across an async operation in `wait_notify` (only to clone `Arc<Notify>`,
  then released — but the acquisition is from an async context)
- P3 forbids blocking operations on async threads; `std::sync::Mutex::lock()` is not blocking
  *if not contended*, but best practice in tokio is to use `tokio::sync::Mutex` for mutexes
  held in async contexts

**Critical implementation note**: In `wait_notify`, the lock MUST be released before calling
`notify.notified().await`. If the lock is held while awaiting, `push` (called from another task)
cannot acquire the lock to trigger the notification — causing a deadlock. The implementation
acquires the lock, clones the `Arc<Notify>`, drops the lock, then awaits.

**Alternatives considered**:
1. *`std::sync::Mutex`*: Acceptable for short-lived locks that don't cross await points (if the
   code is written carefully). Rejected in favor of `tokio::sync::Mutex` for consistency with
   the rest of the codebase (`HubState` uses `tokio::sync::Mutex` throughout) and to make the
   "don't hold lock across await" constraint easier to enforce.
2. *`RwLock`*: Read-heavy operations (drain, notify_handle, queue_sizes) could benefit. Rejected
   for now — the locking critical sections are extremely short; the optimization is premature.
   Can be added later if benchmarks show contention.

---

## Summary: All Unknowns Resolved

| Unknown | Resolution |
|---------|------------|
| `async fn in trait` object safety | `async-trait` crate |
| Error type | Extend `HubError` with `QueueBackend(String)` |
| Notification abstraction | `async fn wait_notify(vtoken, timeout) -> bool` |
| `ensure` on trait | Removed; absorbed into `push`/`wait_notify` |
| Interior mutability | `tokio::sync::Mutex` inside `InMemoryQueue` |
| Lock safety for `wait_notify` | Clone `Arc<Notify>`, release lock, then await |
