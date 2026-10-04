//! Per-client message queue — trait-based abstraction with in-memory default.
//!
//! The [`MessageQueue`] trait defines the contract for all queue backends.
//! [`InMemoryQueue`] is the default implementation backed by a `DashMap` with per-slot synchronous `std::sync::Mutex`.

use async_trait::async_trait;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::warn;

use crate::error::HubError;
use crate::ilink::types::WeixinMessage;

/// Default maximum number of messages buffered per client.
pub const DEFAULT_MAX_QUEUE_SIZE: usize = 200;

// ─── MessageQueue trait ───────────────────────────────────────────────────────

/// Abstraction over a message queue backend for iLink Hub.
///
/// # Object Safety
///
/// This trait is object-safe and intended to be used as `Arc<dyn MessageQueue>`.
///
/// # Downstream Crate Integration
///
/// Downstream crates can implement this trait for custom backends (e.g. Redis)
/// and inject them into [`crate::hub::HubState`]:
///
/// ```ignore
/// use ilink_hub::{MessageQueue, PollBatch};
/// use ilink_hub::hub::HubState;
/// use ilink_hub::error::HubError;
/// use ilink_hub::ilink::types::WeixinMessage;
/// use async_trait::async_trait;
/// use std::collections::HashMap;
/// use std::sync::Arc;
///
/// struct CustomQueue;
///
/// #[async_trait]
/// impl MessageQueue for CustomQueue {
///     async fn push(&self, _vtoken: &str, _msg: WeixinMessage) -> Result<bool, HubError> {
///         Ok(false)
///     }
///     async fn poll(&self, _vtoken: &str, _ack: Option<u64>) -> Result<PollBatch, HubError> {
///         Ok(PollBatch { msgs: vec![], cursor: 0 })
///     }
///     async fn wait_notify(&self, _vtoken: &str, _timeout_secs: u64) -> Result<bool, HubError> {
///         Ok(false)
///     }
///     async fn remove_client(&self, _vtoken: &str) -> Result<(), HubError> {
///         Ok(())
///     }
///     async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
///         Ok(HashMap::new())
///     }
/// }
/// ```
/// One delivery batch returned by [`MessageQueue::poll`].
///
/// `msgs` holds **every still-unacknowledged** message for the vtoken, in FIFO
/// order (delivery is at-least-once: a batch is only retired when the client
/// echoes `cursor` back on its next poll). `cursor` is the delivery high-water
/// mark the client must return verbatim to acknowledge the batch.
#[derive(Debug, Clone, Default)]
pub struct PollBatch {
    pub msgs: Vec<WeixinMessage>,
    pub cursor: u64,
}

impl PollBatch {
    /// True when no unacknowledged message is pending for this vtoken.
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }
}

#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Enqueue `msg` for `vtoken`.
    ///
    /// Return value is the **backpressure** flag, not a delivery verdict:
    ///
    /// * `Ok(false)` — the message was enqueued.
    /// * `Ok(true)` — the queue is full and the message was **rejected**; the
    ///   oldest queued message is retained (never silently dropped) and the
    ///   caller should surface the rejection (see
    ///   `Metrics::messages_rejected_by_client`).
    ///
    /// Implementations must stamp a per-vtoken stable delivery id
    /// ([`WeixinMessage::seq`]) on every accepted message so redelivery of the
    /// same message carries the same id.
    async fn push(&self, vtoken: &str, msg: WeixinMessage) -> Result<bool, HubError>;
    /// Optimised push for the broadcast path: the base message is shared via
    /// `Arc<WeixinMessage>` and only the per-recipient `context_token` and
    /// `ilink_hub_ext` are supplied separately. The base clone cost drops from
    /// O(N × msg_size) to O(msg_size) + N × cheap field clone, which matters
    /// when many backends are online and a message carries images / files.
    ///
    /// The default implementation clones the base and overlays the overrides,
    /// so implementations that don't care about the optimisation still work.
    async fn push_shared(
        &self,
        vtoken: &str,
        base: Arc<WeixinMessage>,
        context_token: Option<String>,
        hub_ext: Option<crate::ilink::types::HubExt>,
    ) -> Result<bool, HubError> {
        let mut msg = (*base).clone();
        msg.context_token = context_token;
        msg.ilink_hub_ext = hub_ext;
        self.push(vtoken, msg).await
    }
    /// Non-destructive read of the unacknowledged batch for `vtoken`.
    ///
    /// `ack` is the cursor the client echoed back from a previous response (or
    /// `None` when it did not echo one). It acknowledges — and only then retires —
    /// every message whose stable id is `<= ack`. Messages above the watermark,
    /// and every message when `ack` is `None`, stay queued and are returned
    /// again by the next poll. That is what makes delivery at-least-once: a
    /// response lost on the way back is simply redelivered.
    ///
    /// Implementations must clamp `ack` to the highest id already allocated, so
    /// a forged or stale cursor can neither retire unallocated messages nor
    /// silence future ones.
    async fn poll(&self, vtoken: &str, ack: Option<u64>) -> Result<PollBatch, HubError>;
    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError>;
    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError>;
    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError>;
}

// ─── InMemoryQueue ────────────────────────────────────────────────────────────
//
// Design: DashMap for lock-free per-client slot lookup, std::sync::Mutex per slot
// for the message buffer. N concurrent long-polls for different clients never
// block each other — only same-client operations briefly contend.
//
// `wait_notify` clones Arc<Notify> and releases all locks before awaiting, so
// N simultaneous long-polls hold zero shared locks while waiting.

/// Buffer plus delivery-id allocator, guarded by a single mutex so that
/// allocating an id, appending, and retiring an acknowledged prefix are one
/// atomic step.
struct SlotState {
    messages: VecDeque<WeixinMessage>,
    /// Next delivery id to hand out (1-based, monotonic per vtoken).
    next_seq: u64,
}

struct PerClientSlot {
    state: std::sync::Mutex<SlotState>,
    notify: Arc<Notify>,
    max_queue_size: usize,
}

impl PerClientSlot {
    fn new(max_queue_size: usize) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(SlotState {
                messages: VecDeque::new(),
                next_seq: 1,
            }),
            notify: Arc::new(Notify::new()),
            max_queue_size,
        })
    }

    /// Enqueue unless the buffer is full. Returns `true` when the new message
    /// was rejected (backpressure) — the oldest queued message is kept.
    fn push(&self, mut msg: WeixinMessage) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.messages.len() >= self.max_queue_size {
            warn!(
                max = self.max_queue_size,
                "client queue full, rejecting new message (backpressure)"
            );
            return true;
        }
        msg.seq = Some(st.next_seq as i64);
        st.next_seq += 1;
        st.messages.push_back(msg);
        self.notify.notify_one();
        false
    }

    /// Retire the acknowledged prefix, then return everything still pending.
    ///
    /// `ack` is clamped to the highest allocated id: a client that acks by
    /// `message_id` (a different, non-contiguous space) can never retire
    /// messages the Hub has not yet handed out, and a forged large cursor
    /// cannot silence messages pushed later.
    fn poll(&self, ack: Option<u64>) -> PollBatch {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let allocated_max = st.next_seq.saturating_sub(1);
        let watermark = ack.map(|a| a.min(allocated_max));

        if let Some(watermark) = watermark {
            while st
                .messages
                .front()
                .and_then(|m| m.seq)
                .is_some_and(|seq| seq >= 0 && seq as u64 <= watermark)
            {
                st.messages.pop_front();
            }
        }

        let msgs: Vec<WeixinMessage> = st.messages.iter().cloned().collect();
        let cursor = msgs
            .last()
            .and_then(|m| m.seq)
            .and_then(|seq| u64::try_from(seq).ok())
            .or(watermark)
            .unwrap_or(allocated_max);
        PollBatch { msgs, cursor }
    }

    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .messages
            .len()
    }
}

pub struct InMemoryQueue {
    slots: DashMap<String, Arc<PerClientSlot>>,
    max_queue_size: usize,
}

impl InMemoryQueue {
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_MAX_QUEUE_SIZE)
    }

    pub fn with_limit(max_queue_size: usize) -> Self {
        Self {
            slots: DashMap::new(),
            max_queue_size,
        }
    }

    fn get_or_create(&self, vtoken: &str) -> Arc<PerClientSlot> {
        self.slots
            .entry(vtoken.to_string())
            .or_insert_with(|| PerClientSlot::new(self.max_queue_size))
            .clone()
    }
}

impl Default for InMemoryQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessageQueue for InMemoryQueue {
    async fn push(&self, vtoken: &str, msg: WeixinMessage) -> Result<bool, HubError> {
        Ok(self.get_or_create(vtoken).push(msg))
    }

    async fn push_shared(
        &self,
        vtoken: &str,
        base: Arc<WeixinMessage>,
        context_token: Option<String>,
        hub_ext: Option<crate::ilink::types::HubExt>,
    ) -> Result<bool, HubError> {
        // Specialised path: clone the base, overlay only the two per-recipient
        // fields, then push. `WeixinMessage::item_list` is `Arc<Vec<…>>` so
        // its clone cost is shared with the broadcast source; the
        // `context_token` and `ilink_hub_ext` are the only per-recipient
        // allocations.
        let mut msg = (*base).clone();
        msg.context_token = context_token;
        msg.ilink_hub_ext = hub_ext;
        Ok(self.get_or_create(vtoken).push(msg))
    }

    async fn poll(&self, vtoken: &str, ack: Option<u64>) -> Result<PollBatch, HubError> {
        // Deliberately no `get_or_create`: polling an unknown vtoken must not
        // materialise a slot. An empty batch carries the neutral cursor 0.
        Ok(self
            .slots
            .get(vtoken)
            .map(|s| s.poll(ack))
            .unwrap_or_default())
    }

    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError> {
        // Clone Arc<Notify> and release the DashMap shard lock before awaiting.
        let notify = self.get_or_create(vtoken).notify.clone();
        let result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), notify.notified()).await;
        Ok(result.is_ok())
    }

    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError> {
        self.slots.remove(vtoken);
        Ok(())
    }

    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
        Ok(self
            .slots
            .iter()
            .map(|e| (e.key().clone(), e.value().len()))
            .collect())
    }
}

#[cfg(test)]
mod queue_config_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_in_memory_queue_with_limit() {
        let q = InMemoryQueue::with_limit(10);
        let vtoken = "v1";

        // Push 10 messages, no drops
        for i in 0..10 {
            let msg = WeixinMessage {
                message_id: Some(i),
                ..Default::default()
            };
            let dropped = q.push(vtoken, msg).await.unwrap();
            assert!(!dropped);
        }

        // Push 11th message: rejected (backpressure), oldest retained.
        let msg = WeixinMessage {
            message_id: Some(10),
            ..Default::default()
        };
        let rejected = q.push(vtoken, msg).await.unwrap();
        assert!(rejected);

        let batch = q.poll(vtoken, None).await.unwrap();
        assert_eq!(batch.msgs.len(), 10);
        assert_eq!(batch.msgs[0].message_id, Some(0));
        assert_eq!(batch.msgs[9].message_id, Some(9));
    }

    #[tokio::test]
    async fn test_push_shared_overrides_context_and_hub_ext_per_recipient() {
        use crate::ilink::types::HubExt;
        let q = InMemoryQueue::new();
        let base = Arc::new(WeixinMessage {
            from_user_id: Some("user-1".into()),
            context_token: Some("shared".into()),
            ..Default::default()
        });

        // Two recipients should see the shared fields preserved, but their
        // own context_token and hub_ext applied.
        q.push_shared(
            "v1",
            Arc::clone(&base),
            Some("vctx-v1".into()),
            Some(HubExt {
                session_id: Some("sid-v1".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        q.push_shared(
            "v2",
            Arc::clone(&base),
            Some("vctx-v2".into()),
            Some(HubExt {
                session_id: Some("sid-v2".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let v1 = q.poll("v1", None).await.unwrap().msgs;
        let v2 = q.poll("v2", None).await.unwrap().msgs;
        assert_eq!(v1.len(), 1);
        assert_eq!(v2.len(), 1);
        assert_eq!(v1[0].context_token.as_deref(), Some("vctx-v1"));
        assert_eq!(v2[0].context_token.as_deref(), Some("vctx-v2"));
        // Shared field is preserved across recipients.
        assert_eq!(v1[0].from_user_id.as_deref(), Some("user-1"));
        assert_eq!(v2[0].from_user_id.as_deref(), Some("user-1"));
        // Per-recipient hub_ext is preserved.
        assert_eq!(
            v1[0]
                .ilink_hub_ext
                .as_ref()
                .and_then(|e| e.session_id.as_deref()),
            Some("sid-v1")
        );
        assert_eq!(
            v2[0]
                .ilink_hub_ext
                .as_ref()
                .and_then(|e| e.session_id.as_deref()),
            Some("sid-v2")
        );
    }

    #[test]
    fn test_mutex_poison_safe() {
        use std::thread;

        // Test InMemoryQueue (PerClientSlot) poison safety
        let slot = Arc::new(PerClientSlot::new(10));
        let slot_clone = slot.clone();
        let handle3 = thread::spawn(move || {
            let _lock = slot_clone.state.lock().unwrap();
            panic!("force panic to poison PerClientSlot Mutex");
        });
        let _ = handle3.join();

        // Now test push/poll/len on the poisoned slot should not panic and should behave correctly
        assert!(!slot.push(WeixinMessage::default()));
        assert_eq!(slot.len(), 1);
        let first_batch = slot.poll(None);
        assert_eq!(first_batch.msgs.len(), 1);
        assert!(slot.poll(Some(first_batch.cursor)).msgs.is_empty());
        assert_eq!(slot.len(), 0);

        // Push multiple messages into the poisoned slot
        for i in 0..5 {
            let msg = WeixinMessage {
                message_id: Some(i),
                ..Default::default()
            };
            slot.push(msg);
        }
        assert_eq!(slot.len(), 5);
        let batch = slot.poll(None);
        assert_eq!(batch.msgs.len(), 5);
        assert_eq!(batch.msgs[0].message_id, Some(0));
        // Non-destructive: without an ack the messages stay queued.
        assert_eq!(slot.len(), 5);
        assert!(slot.poll(Some(batch.cursor)).msgs.is_empty());
        assert_eq!(slot.len(), 0);

        // Concurrent adversarial test on poisoned PerClientSlot
        let mut slot_handles = vec![];
        for thread_idx in 0..10 {
            let slot_thread = slot.clone();
            slot_handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let msg = WeixinMessage {
                        message_id: Some(thread_idx * 100 + i),
                        ..Default::default()
                    };
                    slot_thread.push(msg);
                    let batch = slot_thread.poll(None);
                    for m in batch.msgs {
                        assert!(m.message_id.is_some());
                    }
                }
            }));
        }
        for h in slot_handles {
            h.join().unwrap();
        }
    }

    struct AlwaysFalseQueue;

    #[async_trait::async_trait]
    impl crate::MessageQueue for AlwaysFalseQueue {
        async fn push(
            &self,
            _vtoken: &str,
            _msg: crate::ilink::types::WeixinMessage,
        ) -> Result<bool, crate::error::HubError> {
            Ok(false)
        }
        async fn poll(
            &self,
            _vtoken: &str,
            _ack: Option<u64>,
        ) -> Result<crate::hub::queue::PollBatch, crate::error::HubError> {
            Ok(crate::hub::queue::PollBatch::default())
        }
        async fn wait_notify(
            &self,
            _vtoken: &str,
            _timeout_secs: u64,
        ) -> Result<bool, crate::error::HubError> {
            Ok(false)
        }
        async fn remove_client(&self, _vtoken: &str) -> Result<(), crate::error::HubError> {
            Ok(())
        }
        async fn queue_sizes(
            &self,
        ) -> Result<std::collections::HashMap<String, usize>, crate::error::HubError> {
            Ok(std::collections::HashMap::new())
        }
    }

    struct AlwaysTrueQueue;

    #[async_trait::async_trait]
    impl crate::MessageQueue for AlwaysTrueQueue {
        async fn push(
            &self,
            _vtoken: &str,
            _msg: crate::ilink::types::WeixinMessage,
        ) -> Result<bool, crate::error::HubError> {
            Ok(true)
        }
        async fn poll(
            &self,
            _vtoken: &str,
            _ack: Option<u64>,
        ) -> Result<crate::hub::queue::PollBatch, crate::error::HubError> {
            Ok(crate::hub::queue::PollBatch::default())
        }
        async fn wait_notify(
            &self,
            _vtoken: &str,
            _timeout_secs: u64,
        ) -> Result<bool, crate::error::HubError> {
            Ok(false)
        }
        async fn remove_client(&self, _vtoken: &str) -> Result<(), crate::error::HubError> {
            Ok(())
        }
        async fn queue_sizes(
            &self,
        ) -> Result<std::collections::HashMap<String, usize>, crate::error::HubError> {
            Ok(std::collections::HashMap::new())
        }
    }

    #[tokio::test]
    async fn push_shared_default_propagates_false_from_push() {
        let queue = AlwaysFalseQueue;
        let base = Arc::new(WeixinMessage::default());
        let result = queue.push_shared("v1", base, None, None).await.unwrap();
        assert!(
            !result,
            "push_shared default impl must propagate Ok(false) from push()"
        );
    }

    #[tokio::test]
    async fn push_shared_default_propagates_true_from_push() {
        let queue = AlwaysTrueQueue;
        let base = Arc::new(WeixinMessage::default());
        let result = queue.push_shared("v1", base, None, None).await.unwrap();
        assert!(
            result,
            "push_shared default impl must propagate the Ok(true) backpressure rejection from push()"
        );
    }

    /// Acceptance: a poll without an ack must redeliver the same ids, so a
    /// response lost on the way back cannot lose the message.
    #[tokio::test]
    async fn poll_without_ack_redelivers_same_ids() {
        let q = InMemoryQueue::new();
        for i in 0..3 {
            q.push(
                "v1",
                WeixinMessage {
                    message_id: Some(i),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let first = q.poll("v1", None).await.unwrap();
        assert_eq!(first.msgs.len(), 3);
        let second = q.poll("v1", None).await.unwrap();
        assert_eq!(
            second.msgs.iter().map(|m| m.seq).collect::<Vec<_>>(),
            first.msgs.iter().map(|m| m.seq).collect::<Vec<_>>(),
            "redelivery must reuse the same delivery ids"
        );
        assert_eq!(second.cursor, first.cursor);
    }

    /// Acceptance: echoing the cursor retires exactly the delivered prefix.
    #[tokio::test]
    async fn poll_with_ack_prunes_acked_prefix() {
        let q = InMemoryQueue::new();
        q.push(
            "v1",
            WeixinMessage {
                message_id: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = q.poll("v1", None).await.unwrap();
        assert_eq!(first.cursor, 1);

        q.push(
            "v1",
            WeixinMessage {
                message_id: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let second = q.poll("v1", Some(first.cursor)).await.unwrap();
        assert_eq!(second.msgs.len(), 1, "only the unacked message remains");
        assert_eq!(second.msgs[0].message_id, Some(1));
        assert_eq!(second.cursor, 2);
        assert_eq!(q.queue_sizes().await.unwrap()["v1"], 1);
    }
}
