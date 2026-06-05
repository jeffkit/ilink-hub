# Data Model: MessageQueue Trait Abstraction

**Feature**: `feat/message-queue-trait`  
**Date**: 2026-06-05

---

## Entities

### `MessageQueue` (trait — public interface)

| Aspect | Value |
|--------|-------|
| Kind | Rust trait (public, library-facing) |
| Module | `ilink_hub::hub::queue` |
| Public export | `ilink_hub::MessageQueue` (via `src/lib.rs`) |
| Object safety | ✅ (via `async-trait`) |
| Send + Sync | Required — used as `Arc<dyn MessageQueue + Send + Sync>` |

**Methods** (all async via `async-trait`):

| Method | Signature | Description |
|--------|-----------|-------------|
| `push` | `(&self, vtoken: &str, msg: InboundMessage) → Result<(), HubError>` | Buffer a message; auto-create queue; head-drop at capacity |
| `drain` | `(&self, vtoken: &str) → Result<Vec<InboundMessage>, HubError>` | Retrieve and clear all pending messages |
| `wait_notify` | `(&self, vtoken: &str, timeout_secs: u64) → Result<bool, HubError>` | Block until notification or timeout; returns `true` if notified |
| `queue_sizes` | `(&self) → Result<HashMap<String, usize>, HubError>` | Current pending count per vtoken |
| `remove_client` | `(&self, vtoken: &str) → Result<(), HubError>` | Remove queue and notifier for disconnected client |

---

### `InMemoryQueue` (concrete implementation — public struct)

| Aspect | Value |
|--------|-------|
| Kind | Rust struct implementing `MessageQueue` |
| Module | `ilink_hub::hub::queue` |
| Public export | `ilink_hub::hub::InMemoryQueue` |
| Renamed from | `QueueStore` |
| Internal state | `tokio::sync::Mutex<HashMap<String, ClientQueue>>` |
| Max queue size | `MAX_QUEUE_SIZE = 200` (constant, preserved) |

**Fields** (internal, not public):

| Field | Type | Description |
|-------|------|-------------|
| `queues` | `tokio::sync::Mutex<HashMap<String, ClientQueue>>` | Per-vtoken queue map; protected by async mutex |

**Overflow policy**: When `pending.len() >= MAX_QUEUE_SIZE` during `push`, call `pending.pop_front()` (drop oldest) before `push_back` (add newest). Log `WARN` with `max = MAX_QUEUE_SIZE`. Increment `metrics.messages_dropped` counter.

> Note: The `messages_dropped` increment lives in the caller (`hub/mod.rs` dispatcher), not inside the trait implementation, to keep the trait decoupled from the `Metrics` struct.

---

### `ClientQueue` (internal — unchanged)

| Aspect | Value |
|--------|-------|
| Kind | Rust struct (internal to `InMemoryQueue`) |
| Visibility | `pub(crate)` or private to module |
| Unchanged | Yes — same as pre-refactor |

**Fields**:

| Field | Type | Description |
|-------|------|-------------|
| `pending` | `VecDeque<InboundMessage>` | Buffered messages for this client |
| `notify` | `Arc<tokio::sync::Notify>` | Notification handle for long-poll wakeup |

---

### `ContextTokenMap` (unchanged)

Not part of the `MessageQueue` trait. Remains a separate struct in `src/hub/queue.rs`.
Spec assumption: "The `ContextTokenMap` is a separate concern and is NOT folded into the `MessageQueue` trait."

---

### `HubState` (updated)

| Field | Before | After |
|-------|--------|-------|
| `queues` | `tokio::sync::Mutex<QueueStore>` | _(field removed)_ |
| `queue` | _(did not exist)_ | `Arc<dyn MessageQueue + Send + Sync>` |

Constructor signature change:
```rust
// Before
pub fn new(upstream: Arc<UpstreamClient>, store: Arc<Store>) -> Arc<Self>

// After  
pub fn new(
    upstream: Arc<UpstreamClient>,
    store: Arc<Store>,
    queue: Arc<dyn MessageQueue + Send + Sync>,
) -> Arc<Self>
```

---

### `HubError` (extended)

New variant added to `src/error.rs`:

```rust
/// Queue backend operation failed.
///
/// Produced by remote backends (e.g., Redis connection failure).
/// `InMemoryQueue` never produces this variant in practice.
#[error("queue backend error: {0}")]
QueueBackend(String),
```

---

### `QueueBackendConfig` (startup logic — not a struct, a function)

Represented as the `build_queue_backend()` function in `src/main.rs`.

**Environment variables read**:

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `ILINK_QUEUE_BACKEND` | `String` | `"memory"` | Selects queue implementation |
| `ILINK_REDIS_URL` | `String` | _(no default)_ | Required if `ILINK_QUEUE_BACKEND=redis` |

**Accepted values for `ILINK_QUEUE_BACKEND`**:

| Value | Behavior |
|-------|----------|
| _(unset)_ | Same as `"memory"` |
| `"memory"` | Creates `InMemoryQueue`; logs `INFO` |
| `"redis"` | Reads `ILINK_REDIS_URL`; fails if absent; fails with "not yet implemented" even if present |
| _(any other)_ | Fails with actionable error listing supported values |

---

## State Transitions

### Queue lifecycle per vtoken

```
[no entry]
    │
    ├─ push(vtoken, msg) ──────────────────► [entry created; msg buffered]
    │                                                │
    ├─ wait_notify(vtoken, t) ─────────────► [entry created; awaiting notify]
    │                                                │
    │                                    push(vtoken, msg)
    │                                                │
    │                                                ▼
    │                                       [entry exists; msg buffered; notify triggered]
    │                                                │
    │                                    drain(vtoken)
    │                                                │
    │                                                ▼
    │                                       [entry exists; queue empty]
    │                                                │
    │                                    remove_client(vtoken)
    │                                                │
    │                                                ▼
    └─────────────────────────────────────── [no entry]
```

### Overflow transition

```
[entry; pending.len() == 200]
    │
    push(vtoken, new_msg)
    │
    ├── pop_front() ─► oldest message silently discarded
    │
    └── push_back(new_msg) ─► [entry; pending.len() == 200; new_msg at tail]
```

### Concurrent push race

```
Task A: push(vtoken, msgA)        Task B: push(vtoken, msgB)
    │                                 │
    lock(queues)                      │ (waiting)
    │                                 │
    push_back(msgA)                   │
    notify_one()                      │
    │                                 │
    unlock(queues)                    lock(queues)
                                      │
                                      push_back(msgB)
                                      notify_one()
                                      │
                                      unlock(queues)
```
Result: Both messages in queue; at least one `notify_one()` fired. Long-pollers wake up and drain both.

---

## Invariants

1. **Queue bound**: `pending.len() ≤ MAX_QUEUE_SIZE (200)` at all times per vtoken.
2. **No panic on remove + concurrent push**: If `remove_client` runs concurrently with `push` for the same vtoken, `push` creates a new entry; there is no crash or data race (mutex serializes access).
3. **Wait-notify lock ordering**: `wait_notify` acquires the lock only to clone `Arc<Notify>`, then releases before awaiting — preventing deadlock with concurrent `push`.
4. **Empty drain**: `drain` on a vtoken with no pending messages returns `Vec::new()`, never an error.
5. **Unknown vtoken on drain**: `drain` on an unregistered vtoken returns `Vec::new()` (no entry created). Consistent with current `QueueStore::drain` behavior.
6. **Object safety**: The `MessageQueue` trait MUST compile as `Arc<dyn MessageQueue + Send + Sync>`. This is enforced by the `test_object_safe` unit test.
