# API Contract: `MessageQueue` Trait

**Feature**: `feat/message-queue-trait`  
**Date**: 2026-06-05  
**Location**: `src/hub/queue.rs`, exported from `src/lib.rs`

This document is the authoritative definition of the `MessageQueue` trait contract.
Implementors of `RedisQueue` or other backends MUST satisfy all behaviors documented here.

---

## Full Trait Definition

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use crate::ilink::types::InboundMessage;
use crate::error::HubError;

/// Abstraction over all queue operations performed on behalf of a registered client.
///
/// Each client is identified by a **vtoken** — an opaque string assigned by the hub
/// (format: `vctx_<uuid_simple>`). All queue operations are keyed by this string.
///
/// Implementations handle buffering, notification, and lifecycle of per-client message
/// queues. The backing store may be in-process (`InMemoryQueue`) or remote (Redis, etc.).
///
/// # Object Safety
///
/// This trait is designed for use as `Arc<dyn MessageQueue + Send + Sync>`.
/// All async methods are object-safe via the `async_trait` attribute macro, which
/// desugars each `async fn` to a `Pin<Box<dyn Future<Output = ...> + Send + '_>>`.
///
/// # Implementing This Trait
///
/// See `quickstart.md` for a step-by-step guide with a minimal mock implementation.
///
/// # Guarantees Required of Implementors
///
/// - `push` MUST be idempotent with respect to queue creation: calling `push` on an
///   unregistered vtoken MUST create the queue entry, not return an error.
/// - `drain` MUST return an empty `Vec` (not an error) for unknown or empty vtokens.
/// - `wait_notify` MUST NOT panic when called for a vtoken with no pending messages.
/// - `remove_client` MUST NOT panic when called for an unknown vtoken (no-op is correct).
/// - All methods MUST be safe to call concurrently from multiple tokio tasks.
#[async_trait]
pub trait MessageQueue: Send + Sync {

    /// Buffer `msg` in the queue identified by `vtoken`.
    ///
    /// # Auto-creation
    ///
    /// If no queue entry exists for `vtoken`, one is created automatically.
    /// Callers do not need to call any initialization method before `push`.
    ///
    /// # Overflow policy
    ///
    /// When the queue is at capacity, the **oldest** message is dropped before
    /// the new message is appended (head-drop / FIFO overflow). The capacity
    /// is determined by each implementation; `InMemoryQueue` uses 200 messages.
    ///
    /// # Errors
    ///
    /// Returns `Err(HubError::QueueBackend(...))` if the backend store is unavailable
    /// (e.g., Redis connection refused). `InMemoryQueue` never returns an error.
    async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<(), HubError>;

    /// Drain and return all pending messages for `vtoken`.
    ///
    /// All messages are removed from the queue atomically. Subsequent calls
    /// with no intervening `push` return an empty `Vec`.
    ///
    /// # Not blocking
    ///
    /// This method returns immediately. To wait for messages to become available,
    /// call [`wait_notify`] first.
    ///
    /// # Unknown vtokens
    ///
    /// If `vtoken` has no queue entry, returns `Ok(vec![])` — not an error.
    ///
    /// # Errors
    ///
    /// Returns `Err(HubError::QueueBackend(...))` on backend failure.
    async fn drain(&self, vtoken: &str) -> Result<Vec<InboundMessage>, HubError>;

    /// Wait until a message is available for `vtoken` or until `timeout_secs` elapses.
    ///
    /// # Return value
    ///
    /// - `Ok(true)` — a notification was received (one or more messages are available);
    ///   the caller should call [`drain`] to retrieve them.
    /// - `Ok(false)` — `timeout_secs` elapsed without any push to this queue.
    ///
    /// # Auto-creation
    ///
    /// If no queue entry exists for `vtoken`, one is created before waiting.
    /// The caller does not need to call any initialization method first.
    ///
    /// # Notification semantics
    ///
    /// Implementations may use any notification primitive (tokio `Notify`,
    /// Redis pub/sub, etc.) internally. There is no guarantee about how many
    /// messages are available when `true` is returned — always call `drain` to
    /// retrieve the full batch.
    ///
    /// # Errors
    ///
    /// Returns `Err(HubError::QueueBackend(...))` on backend failure.
    async fn wait_notify(
        &self,
        vtoken: &str,
        timeout_secs: u64,
    ) -> Result<bool, HubError>;

    /// Return the current number of pending messages per vtoken.
    ///
    /// Used by the `/metrics` endpoint to expose per-client queue depth as a
    /// Prometheus gauge (`ilink_hub_queue_size{client="<name>"}`).
    ///
    /// Returns a snapshot; the values may be stale by the time the caller reads them.
    ///
    /// # Errors
    ///
    /// Returns `Err(HubError::QueueBackend(...))` on backend failure.
    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError>;

    /// Remove the queue and any associated notification state for `vtoken`.
    ///
    /// Called when a client disconnects or is de-registered. After this call:
    /// - `drain(vtoken)` returns `Ok(vec![])`.
    /// - A concurrent `push(vtoken, msg)` creates a new queue entry (no panic).
    ///
    /// # Unknown vtokens
    ///
    /// If `vtoken` has no queue entry, this is a no-op. Not an error.
    ///
    /// # Errors
    ///
    /// Returns `Err(HubError::QueueBackend(...))` on backend failure.
    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError>;
}
```

---

## `InMemoryQueue` Implementation Summary

```rust
/// In-process message queue backed by `VecDeque` and `tokio::sync::Notify`.
///
/// This is the default backend when `ILINK_QUEUE_BACKEND` is unset or `memory`.
/// Requires no external dependencies. All state is lost on process restart.
pub struct InMemoryQueue {
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
}

impl InMemoryQueue {
    /// Create a new empty queue store.
    pub fn new() -> Self {
        Self {
            queues: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}
```

**`push` pseudocode**:
```
lock queues
  get or create ClientQueue for vtoken
  if pending.len() >= MAX_QUEUE_SIZE:
    pending.pop_front()
    log WARN "queue full, dropping oldest"
  pending.push_back(msg)
  notify.notify_one()
unlock
```

**`wait_notify` pseudocode** (lock safety critical):
```
lock queues briefly
  get or create ClientQueue for vtoken
  clone Arc<Notify>  ← MUST copy before releasing lock
unlock                ← MUST release before awaiting

tokio::time::timeout(timeout_secs, notify.notified()).await
→ Ok if notified, Err(Elapsed) if timeout
→ return true / false
```

**`drain` pseudocode**:
```
lock queues
  if no entry for vtoken: return Ok(vec![])
  messages = queue.pending.drain(..).collect()
unlock
return Ok(messages)
```

**`queue_sizes` pseudocode**:
```
lock queues
  snapshot = iter map (vtoken, q) → (vtoken.clone(), q.pending.len())
unlock
return Ok(snapshot)
```

**`remove_client` pseudocode**:
```
lock queues
  remove entry for vtoken (no-op if absent)
unlock
return Ok(())
```

---

## Minimum Mock Implementation (for testing downstream crates)

A downstream implementor can satisfy the trait with a stub:

```rust
use async_trait::async_trait;
use ilink_hub::{MessageQueue, HubError};
use ilink_hub::ilink::types::InboundMessage;
use std::collections::HashMap;

pub struct MockQueue;

#[async_trait]
impl MessageQueue for MockQueue {
    async fn push(&self, _vtoken: &str, _msg: InboundMessage) -> Result<(), HubError> {
        Ok(())
    }
    async fn drain(&self, _vtoken: &str) -> Result<Vec<InboundMessage>, HubError> {
        Ok(vec![])
    }
    async fn wait_notify(&self, _vtoken: &str, timeout_secs: u64) -> Result<bool, HubError> {
        tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
        Ok(false)
    }
    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
        Ok(HashMap::new())
    }
    async fn remove_client(&self, _vtoken: &str) -> Result<(), HubError> {
        Ok(())
    }
}

// Confirm object safety at compile time:
fn _assert_object_safe() {
    let _: std::sync::Arc<dyn MessageQueue + Send + Sync> =
        std::sync::Arc::new(MockQueue);
}
```

---

## Behavioral Contract Matrix

| Scenario | `push` | `drain` | `wait_notify` | `queue_sizes` | `remove_client` |
|----------|--------|---------|---------------|---------------|-----------------|
| Unknown vtoken | Creates entry, buffers msg | Returns `[]` | Creates entry, waits | Does not include vtoken | No-op |
| Known vtoken, empty queue | Buffers msg | Returns `[]` | Waits until push or timeout | Returns `{vtoken: 0}` | Removes entry |
| Known vtoken, N messages | Buffers; drops oldest if N≥cap | Returns N messages, empties queue | Returns `true` immediately if msg exists | Returns `{vtoken: N}` | Removes entry + messages |
| Concurrent push from 2 tasks | Both succeed; serialized by mutex | N/A | N/A | N/A | N/A |
| push after remove_client | Creates fresh entry | N/A | N/A | N/A | N/A |
