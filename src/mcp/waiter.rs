//! A2A (Agent-to-Agent) reply waiter.
//!
//! When `call_agent` is invoked via MCP, the Hub pushes a message into the
//! target Agent's queue and then suspends on a one-shot channel here, waiting
//! for the target's `sendmessage` call to deliver a reply.
//!
//! Key design:
//! - Each pending A2A call is identified by the **target vtoken** + a unique
//!   **call-id** (UUID).  Using a call-id rather than just the vtoken allows
//!   concurrent calls from different callers to the same target to resolve
//!   independently.
//! - The reply text is the raw content of the first text item the target sends
//!   back (the same text that would appear in WeChat).
//! - If `sendmessage` is never called (target offline, timeout, etc.) the
//!   caller's `wait` future resolves with `None` after the timeout.

use dashmap::DashMap;
use tokio::sync::oneshot;

/// Identifier for a pending A2A call.  Stored in `HubExt::a2a_call_id` so
/// the target's `sendmessage` handler can look it up.
pub type CallId = String;

pub struct A2aWaiter {
    /// call_id → sender half of the reply channel.
    pending: DashMap<CallId, oneshot::Sender<String>>,
}

impl A2aWaiter {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
        }
    }

    /// Register a new pending call.  Returns the call-id and the receiver
    /// that will be signalled when the target replies.
    pub fn register(&self) -> (CallId, oneshot::Receiver<String>) {
        let call_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(call_id.clone(), tx);
        (call_id, rx)
    }

    /// Called from `sendmessage` when the target Agent sends its reply.
    /// Returns `true` if the call was still pending (not timed out).
    pub fn resolve(&self, call_id: &str, reply: String) -> bool {
        if let Some((_, tx)) = self.pending.remove(call_id) {
            tx.send(reply).is_ok()
        } else {
            false
        }
    }

    /// Cancel a pending call (e.g. on timeout) so the slot is reclaimed.
    pub fn cancel(&self, call_id: &str) {
        self.pending.remove(call_id);
    }
}

impl Default for A2aWaiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_returns_unique_call_ids() {
        let waiter = A2aWaiter::new();
        let (id1, _rx1) = waiter.register();
        let (id2, _rx2) = waiter.register();
        assert_ne!(id1, id2, "each registration must produce a unique call-id");
    }

    #[tokio::test]
    async fn resolve_delivers_reply_to_receiver() {
        let waiter = A2aWaiter::new();
        let (call_id, rx) = waiter.register();

        let resolved = waiter.resolve(&call_id, "hello from target".to_string());
        assert!(resolved, "resolve must return true for a live registration");

        let reply = rx.await.expect("receiver must get the reply");
        assert_eq!(reply, "hello from target");
    }

    #[tokio::test]
    async fn resolve_returns_false_for_unknown_call_id() {
        let waiter = A2aWaiter::new();
        let resolved = waiter.resolve("nonexistent-call-id", "reply".to_string());
        assert!(!resolved, "resolve on a missing call-id must return false");
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let waiter = A2aWaiter::new();
        let (call_id, rx) = waiter.register();

        waiter.cancel(&call_id);

        // After cancel, resolving must fail (entry removed) and the receiver
        // must immediately observe the sender being dropped (channel closed).
        let resolved = waiter.resolve(&call_id, "late reply".to_string());
        assert!(!resolved, "resolve after cancel must return false");

        // The rx is now disconnected (sender dropped by cancel + DashMap removal)
        assert!(rx.await.is_err(), "receiver must be closed after cancel");
    }

    #[tokio::test]
    async fn resolve_after_timeout_returns_false() {
        // Simulate: caller registers, times out and calls cancel, then the target
        // eventually resolves — must not panic or double-deliver.
        let waiter = A2aWaiter::new();
        let (call_id, rx) = waiter.register();

        // Caller timed out and cancelled.
        waiter.cancel(&call_id);
        drop(rx);

        // Late resolve: sender is already gone.
        let resolved = waiter.resolve(&call_id, "late".to_string());
        assert!(!resolved);
    }

    #[tokio::test]
    async fn multiple_concurrent_registrations_resolve_independently() {
        let waiter = std::sync::Arc::new(A2aWaiter::new());

        let mut handles = vec![];
        for i in 0..10u32 {
            let w = std::sync::Arc::clone(&waiter);
            handles.push(tokio::spawn(async move {
                let (call_id, rx) = w.register();
                let reply_text = format!("reply-{i}");
                let resolved = w.resolve(&call_id, reply_text.clone());
                assert!(resolved, "registration {i} must resolve");
                let got = rx.await.expect("receiver must succeed");
                assert_eq!(got, reply_text, "registration {i} got wrong reply");
            }));
        }

        for h in handles {
            h.await.expect("task must not panic");
        }
    }

    #[test]
    fn default_impl_creates_empty_waiter() {
        let waiter = A2aWaiter::default();
        // Resolving against an empty waiter must return false.
        assert!(!waiter.resolve("any-id", "text".to_string()));
    }
}
