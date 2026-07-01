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
