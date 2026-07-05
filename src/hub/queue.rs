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
/// use ilink_hub::MessageQueue;
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
///     async fn drain(&self, _vtoken: &str) -> Result<Vec<WeixinMessage>, HubError> {
///         Ok(vec![])
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
#[async_trait]
pub trait MessageQueue: Send + Sync {
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
    async fn drain(&self, vtoken: &str) -> Result<Vec<WeixinMessage>, HubError>;
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

struct PerClientSlot {
    messages: std::sync::Mutex<VecDeque<WeixinMessage>>,
    notify: Arc<Notify>,
    max_queue_size: usize,
}

impl PerClientSlot {
    fn new(max_queue_size: usize) -> Arc<Self> {
        Arc::new(Self {
            messages: std::sync::Mutex::new(VecDeque::new()),
            notify: Arc::new(Notify::new()),
            max_queue_size,
        })
    }

    fn push(&self, msg: WeixinMessage) -> bool {
        let mut q = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = if q.len() >= self.max_queue_size {
            q.pop_front();
            warn!(
                max = self.max_queue_size,
                "client queue full, dropping oldest message"
            );
            true
        } else {
            false
        };
        q.push_back(msg);
        self.notify.notify_one();
        dropped
    }

    fn drain(&self) -> Vec<WeixinMessage> {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    fn len(&self) -> usize {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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

    async fn drain(&self, vtoken: &str) -> Result<Vec<WeixinMessage>, HubError> {
        Ok(self
            .slots
            .get(vtoken)
            .map(|s| s.drain())
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

        // Push 11th message, should drop the first one
        let msg = WeixinMessage {
            message_id: Some(10),
            ..Default::default()
        };
        let dropped = q.push(vtoken, msg).await.unwrap();
        assert!(dropped);

        let drained = q.drain(vtoken).await.unwrap();
        assert_eq!(drained.len(), 10);
        assert_eq!(drained[0].message_id, Some(1));
        assert_eq!(drained[9].message_id, Some(10));
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

        let v1 = q.drain("v1").await.unwrap();
        let v2 = q.drain("v2").await.unwrap();
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
            let _lock = slot_clone.messages.lock().unwrap();
            panic!("force panic to poison PerClientSlot Mutex");
        });
        let _ = handle3.join();

        // Now test push/drain/len on the poisoned slot should not panic and should behave correctly
        assert!(!slot.push(WeixinMessage::default()));
        assert_eq!(slot.len(), 1);
        assert_eq!(slot.drain().len(), 1);
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
        let drained = slot.drain();
        assert_eq!(drained.len(), 5);
        assert_eq!(drained[0].message_id, Some(0));
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
                    let drained = slot_thread.drain();
                    for m in drained {
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
        async fn drain(
            &self,
            _vtoken: &str,
        ) -> Result<Vec<crate::ilink::types::WeixinMessage>, crate::error::HubError> {
            Ok(vec![])
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
        async fn drain(
            &self,
            _vtoken: &str,
        ) -> Result<Vec<crate::ilink::types::WeixinMessage>, crate::error::HubError> {
            Ok(vec![])
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
            "push_shared default impl must propagate Ok(true) from push()"
        );
    }
}
