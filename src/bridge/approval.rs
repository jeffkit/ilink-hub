//! Per-session approval inbox used by the `ask` permission strategy.
//!
//! When an agent emits a `permission_request` and the profile's
//! `permission_default` is `ask`, the bridge pauses the turn and asks the user
//! over WeChat. The user's next inbound message on the *same session* must be
//! routed back to that paused turn — not to the normal session worker queue —
//! so it can be parsed as an "allow"/"deny" reply and fed to the agent as a
//! `permission_response`.
//!
//! [`ApprovalBroker`] is the rendezvous point: the paused turn registers an
//! inbox under its session-dispatch key; [`ApprovalBroker::deliver`] is called
//! from `SessionDispatcher::dispatch` *before* normal routing and hands the
//! message to the waiting turn. At most one inbox per session key is expected
//! (the agent is blocked on the response), so a second registration replaces
//! the first and the stale receiver is dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::mpsc;

use crate::ilink::types::WeixinMessage;

/// Capacity of a single approval inbox. The ask loop drains messages one at a
/// time; a small buffer is enough to absorb a burst while the loop is busy
/// reprompting. Anything beyond the buffer is left to fall through to the
/// normal session queue (the turn has effectively ended its ask window).
const APPROVAL_INBOX_CAPACITY: usize = 8;

pub(crate) struct ApprovalBroker {
    // std::sync::Mutex is correct here: the critical section contains only
    // HashMap insert/get/remove with no await points.
    inboxes: Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
}

impl ApprovalBroker {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inboxes: Mutex::new(HashMap::new()),
        })
    }

    /// Try to deliver `msg` to a pending approval on `key`.
    ///
    /// Returns `true` if an inbox existed and accepted the message, in which
    /// case the caller MUST NOT route the message to the normal session queue.
    /// Returns `false` when no approval is pending (or the inbox was full /
    /// closed), so the caller falls through to normal dispatch.
    ///
    /// Borrows `msg` so the caller retains ownership for the fall-through
    /// path. The clone happens at most once, only when an inbox is hit — the
    /// approval path is low-frequency (agent-initiated), so this is cheap.
    ///
    /// Uses `try_send` (non-async) so this can be called from the synchronous
    /// dispatch hot path without holding a lock across an await.
    pub(crate) fn deliver(&self, key: &str, msg: &WeixinMessage) -> bool {
        let tx = self
            .inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned();
        match tx {
            Some(tx) => tx.try_send(msg.clone()).is_ok(),
            None => false,
        }
    }

    /// Register a fresh approval inbox for `key`, replacing any stale one.
    ///
    /// Returns the receiver to await replies on and a guard whose `Drop`
    /// removes the inbox from the map (so later messages resume normal
    /// routing). The guard holds a weak broker ref so dropping the broker
    /// never blocks turn cleanup.
    pub(crate) fn register(
        self: &Arc<Self>,
        key: String,
    ) -> (mpsc::Receiver<WeixinMessage>, ApprovalGuard) {
        let (tx, rx) = mpsc::channel(APPROVAL_INBOX_CAPACITY);
        self.inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), tx);
        (
            rx,
            ApprovalGuard {
                broker: Arc::downgrade(self),
                key,
            },
        )
    }
}

/// RAII guard that removes an approval inbox when the ask window ends
/// (resolved, reprompt-exhausted, or timed out).
pub(crate) struct ApprovalGuard {
    broker: Weak<ApprovalBroker>,
    key: String,
}

impl ApprovalGuard {
    /// Forget the guard without removing the inbox. Used when the broker
    /// itself has already been dropped (e.g. bridge shutdown), where removal
    /// would be a no-op anyway.
    #[allow(dead_code)]
    pub fn forget(self) {
        std::mem::forget(self);
    }
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        if let Some(broker) = self.broker.upgrade() {
            let mut map = broker.inboxes.lock().unwrap_or_else(|e| e.into_inner());
            // Only remove if it still points at *our* inbox — a later
            // register() may have replaced it, and we must not evict the
            // newer registration.
            if map.get(&self.key).is_some() {
                map.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem, WeixinMessage};

    fn msg_with(text: &str) -> WeixinMessage {
        WeixinMessage {
            context_token: Some("ctx".to_string()),
            ilink_hub_ext: Some(crate::ilink::types::HubExt {
                session_name: Some("s".to_string()),
                ..Default::default()
            }),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some(text.to_string()),
                }),
                ..Default::default()
            }])),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn deliver_returns_false_when_no_inbox() {
        let broker = ApprovalBroker::new();
        assert!(!broker.deliver("missing", &msg_with("允许")));
    }

    #[tokio::test]
    async fn register_then_deliver_routes_to_receiver() {
        let broker = ApprovalBroker::new();
        let (mut rx, _guard) = broker.register("k".to_string());
        assert!(broker.deliver("k", &msg_with("允许")));
        let got = rx.recv().await.expect("msg delivered");
        assert_eq!(got.text(), Some("允许"));
    }

    #[tokio::test]
    async fn guard_drop_removes_inbox() {
        let broker = ApprovalBroker::new();
        {
            let (_rx, _guard) = broker.register("k".to_string());
            assert!(broker.deliver("k", &msg_with("允许")));
        }
        // Guard dropped: later messages fall through.
        assert!(!broker.deliver("k", &msg_with("拒绝")));
    }

    #[tokio::test]
    async fn second_register_replaces_first() {
        let broker = ApprovalBroker::new();
        let (_rx1, _guard1) = broker.register("k".to_string());
        let (mut rx2, _guard2) = broker.register("k".to_string());
        assert!(broker.deliver("k", &msg_with("拒绝")));
        assert_eq!(rx2.recv().await.unwrap().text(), Some("拒绝"));
    }
}
