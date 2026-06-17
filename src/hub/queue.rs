//! Per-client message queue — trait-based abstraction with in-memory default.
//!
//! The [`MessageQueue`] trait defines the contract for all queue backends.
//! [`InMemoryQueue`] is the default implementation backed by a `tokio::sync::Mutex`.

use async_trait::async_trait;
use dashmap::DashMap;
use lru::LruCache;
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::warn;
use uuid::Uuid;

use crate::error::HubError;
use crate::ilink::types::WeixinMessage;

/// Default maximum number of messages buffered per client.
pub const DEFAULT_MAX_QUEUE_SIZE: usize = 200;
#[allow(dead_code)]
const MAX_QUEUE_SIZE: usize = DEFAULT_MAX_QUEUE_SIZE;

/// Maximum number of virtual context token mappings held in memory.
/// Oldest entries are evicted when the limit is reached (LRU policy).
const MAX_CTX_MAP_ENTRIES: usize = 50_000;

// ─── context_token mapping ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ContextRecord {
    pub real_token: String,
    pub peer_user_id: String,
    pub conv_key: Option<String>,
}

/// Maps virtual context tokens (issued to clients) to real context tokens
/// (from actual iLink upstream). Clients never see the real tokens.
/// Also stores the peer_user_id (WeChat sender) so the hub can set `to_user_id` on replies.
///
/// WeChat may issue a **new** `real_ctx` on every inbound message even in the same DM.
/// `conv_to_v` keeps one stable virtual token per conversation so backend session IDs
/// (e.g. Claude `--resume`) survive across messages.
///
/// The underlying `LruCache` is capped at [`MAX_CTX_MAP_ENTRIES`] with LRU eviction to bound
/// memory usage during long-running deployments.
pub struct ContextTokenMap {
    inner: std::sync::Mutex<ContextTokenMapInner>,
}

struct ContextTokenMapInner {
    v_to_record: LruCache<String, ContextRecord>,
    real_to_v: LruCache<String, String>,
    conv_to_v: LruCache<String, String>,
}

impl ContextTokenMapInner {
    /// Remove secondary-map entries for a (vtoken, record) pair, guarding against
    /// overwrites (only remove if the index still points at this vtoken).
    fn remove_secondary(&mut self, vtoken: &str, record: &ContextRecord) {
        if self
            .real_to_v
            .peek(&record.real_token)
            .is_some_and(|v| v == vtoken)
        {
            self.real_to_v.pop(&record.real_token);
        }
        if let Some(ref k) = record.conv_key {
            if self.conv_to_v.peek(k).is_some_and(|v| v == vtoken) {
                self.conv_to_v.pop(k);
            }
        }
    }

    fn insert_record(&mut self, vtoken: String, record: ContextRecord) {
        // If the key already exists, pop it to clean up the secondary maps.
        if let Some(old_record) = self.v_to_record.pop(&vtoken) {
            self.remove_secondary(&vtoken, &old_record);
        }

        // Push the new record and handle LRU eviction if we exceed capacity.
        if let Some((evicted_vtoken, evicted_record)) =
            self.v_to_record.push(vtoken.clone(), record.clone())
        {
            self.remove_secondary(&evicted_vtoken, &evicted_record);
        }

        // Insert new indices.
        self.real_to_v
            .put(record.real_token.clone(), vtoken.clone());
        if let Some(ref k) = record.conv_key {
            self.conv_to_v.put(k.clone(), vtoken);
        }
    }

    fn seed_record(
        &mut self,
        vtoken: String,
        real_token: String,
        peer_user_id: String,
        conv_key: Option<String>,
    ) {
        let old_record = self.v_to_record.peek(&vtoken).cloned();
        if let Some(record) = old_record {
            let mut real_changed = false;
            let mut old_real = String::new();
            if record.real_token != real_token {
                real_changed = true;
                old_real = record.real_token;
            }

            let mut conv_changed = false;
            let mut old_conv = None;
            if let Some(ref k) = conv_key {
                if record.conv_key.as_ref() != Some(k) {
                    conv_changed = true;
                    old_conv = record.conv_key;
                }
            }

            if real_changed {
                if self.real_to_v.peek(&old_real) == Some(&vtoken) {
                    self.real_to_v.pop(&old_real);
                }
                self.real_to_v.put(real_token.clone(), vtoken.clone());
            }

            if conv_changed {
                if let Some(ref old_k) = old_conv {
                    if self.conv_to_v.peek(old_k) == Some(&vtoken) {
                        self.conv_to_v.pop(old_k);
                    }
                }
                if let Some(ref k) = conv_key {
                    self.conv_to_v.put(k.clone(), vtoken.clone());
                }
            }

            // Finally, perform the update and promotion
            if let Some(r) = self.v_to_record.get_mut(&vtoken) {
                if real_changed {
                    r.real_token = real_token;
                }
                if !peer_user_id.is_empty() && r.peer_user_id != peer_user_id {
                    r.peer_user_id = peer_user_id;
                }
                if conv_changed {
                    r.conv_key = conv_key;
                }
            }
        } else {
            // Insert fresh record.
            let record = ContextRecord {
                real_token,
                peer_user_id,
                conv_key,
            };
            self.insert_record(vtoken, record);
        }
    }
}

/// Conversation identity for session continuity (DM vs group).
pub fn conversation_key(peer_user_id: &str, group_id: Option<&str>) -> Option<String> {
    if let Some(g) = group_id.filter(|s| !s.is_empty()) {
        return Some(format!("group:{g}"));
    }
    if !peer_user_id.is_empty() {
        return Some(format!("peer:{peer_user_id}"));
    }
    None
}

impl Default for ContextTokenMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextTokenMap {
    pub fn new() -> Self {
        let cap = NonZeroUsize::new(MAX_CTX_MAP_ENTRIES).unwrap();
        Self {
            inner: std::sync::Mutex::new(ContextTokenMapInner {
                v_to_record: LruCache::new(cap),
                real_to_v: LruCache::new(cap),
                conv_to_v: LruCache::new(cap),
            }),
        }
    }

    pub fn has_conversation(&self, conv_key: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.conv_to_v.contains(conv_key)
    }

    /// Seed a known conversation → vctx mapping (e.g. after DB warm-up on hub restart).
    pub fn seed_conversation(
        &self,
        conv_key: String,
        vctx: String,
        real_ctx: String,
        peer_user_id: String,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.seed_record(vctx, real_ctx, peer_user_id, Some(conv_key));
    }

    pub fn map(&self, real_token: String, peer_user_id: String, group_id: Option<&str>) -> String {
        self.map_scoped(real_token, peer_user_id, group_id, None)
    }

    /// Like `map`, but scopes the stable vctx to a specific client (`client_scope`).
    /// Used in broadcast so each backend gets its own independent vctx.
    pub fn map_scoped(
        &self,
        real_token: String,
        peer_user_id: String,
        group_id: Option<&str>,
        client_scope: Option<&str>,
    ) -> String {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conv_key = conversation_key(&peer_user_id, group_id).map(|k| match client_scope {
            Some(scope) => format!("{k}@{scope}"),
            None => k,
        });

        if let Some(ref key) = conv_key {
            if let Some(vtoken) = inner.conv_to_v.peek(key).cloned() {
                let old_record = inner.v_to_record.peek(&vtoken).cloned();
                if let Some(record) = old_record {
                    let mut real_changed = false;
                    let mut old_real = String::new();
                    if record.real_token != real_token {
                        real_changed = true;
                        old_real = record.real_token;
                    }

                    if real_changed {
                        if inner.real_to_v.peek(&old_real) == Some(&vtoken) {
                            inner.real_to_v.pop(&old_real);
                        }
                        inner.real_to_v.put(real_token.clone(), vtoken.clone());
                    }

                    if let Some(r) = inner.v_to_record.get_mut(&vtoken) {
                        if real_changed {
                            r.real_token = real_token;
                        }
                        if !peer_user_id.is_empty() {
                            r.peer_user_id = peer_user_id;
                        }
                    }
                }
                return vtoken;
            }
        }

        // For unscoped calls, also check the real_to_v index.
        if client_scope.is_none() {
            if let Some(vtoken) = inner.real_to_v.peek(&real_token).cloned() {
                if let Some(record) = inner.v_to_record.get_mut(&vtoken) {
                    if !peer_user_id.is_empty() {
                        record.peer_user_id = peer_user_id;
                    }
                }
                return vtoken;
            }
        }

        let vtoken = format!("vctx_{}", Uuid::new_v4().simple());
        let record = ContextRecord {
            real_token,
            peer_user_id,
            conv_key,
        };
        inner.insert_record(vtoken.clone(), record);
        vtoken
    }

    /// Number of virtual context token entries currently held in memory.
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.v_to_record.len()
    }

    /// Whether the map currently holds no entries.
    pub fn is_empty(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.v_to_record.is_empty()
    }

    pub fn resolve(&self, vtoken: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.v_to_record.get(vtoken).map(|r| r.real_token.clone())
    }

    /// Returns `(real_ctx, peer_user_id)` for the given virtual token.
    /// Uses `get` (updates LRU promotion) to correctly update the LRU cache priority.
    pub fn resolve_full(&self, vtoken: &str) -> Option<(String, String)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .v_to_record
            .get(vtoken)
            .map(|r| (r.real_token.clone(), r.peer_user_id.clone()))
    }

    /// Seed a known mapping into the in-memory cache (without peer_user_id).
    pub fn seed(&self, vctx: String, real_ctx: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.seed_record(vctx, real_ctx, "".to_string(), None);
    }

    /// Seed a known mapping including peer_user_id.
    pub fn seed_full(&self, vctx: String, real_ctx: String, peer_user_id: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.seed_record(vctx, real_ctx, peer_user_id, None);
    }
}

#[cfg(test)]
mod context_map_tests {
    use super::ContextTokenMap;

    #[test]
    fn map_creates_stable_virtual_for_same_real() {
        let m = ContextTokenMap::default();
        let v1 = m.map("real-ctx".into(), "user1".into(), None);
        let v2 = m.map("real-ctx".into(), "user1".into(), None);
        assert_eq!(v1, v2);
        assert_eq!(m.resolve(&v1), Some("real-ctx".to_string()));
        assert_eq!(
            m.resolve_full(&v1),
            Some(("real-ctx".to_string(), "user1".to_string()))
        );
    }

    #[test]
    fn map_reuses_vctx_for_same_peer_even_when_real_ctx_changes() {
        let m = ContextTokenMap::default();
        let peer = "user@im.wechat";
        let v1 = m.map("ctx-msg-1".into(), peer.into(), None);
        let v2 = m.map("ctx-msg-2".into(), peer.into(), None);
        assert_eq!(v1, v2);
        assert_eq!(m.resolve(&v2), Some("ctx-msg-2".to_string()));
    }

    #[test]
    fn map_different_real_gets_different_virtual() {
        let m = ContextTokenMap::default();
        let va = m.map("ctx-a".into(), "u".into(), None);
        let vb = m.map("ctx-b".into(), "v".into(), None);
        assert_ne!(va, vb);
    }

    #[test]
    fn seed_full_then_resolve() {
        let m = ContextTokenMap::default();
        m.seed_full("vctx_1".into(), "real".into(), "peer@x".into());
        assert_eq!(m.resolve("vctx_1"), Some("real".to_string()));
        assert_eq!(
            m.resolve_full("vctx_1"),
            Some(("real".to_string(), "peer@x".to_string()))
        );
    }

    #[test]
    fn test_resolve_full_updates_lru_hotness() {
        let m = ContextTokenMap::default();

        let v0 = m.map("real-0".into(), "user-0".into(), None);
        let v1 = m.map("real-1".into(), "user-1".into(), None);

        // Access v0 to promote it
        assert!(m.resolve_full(&v0).is_some());

        // Insert more entries to trigger eviction (MAX_CTX_MAP_ENTRIES is 50,000)
        for i in 2..=50000 {
            m.map(format!("real-{}", i), format!("user-{}", i), None);
        }

        // v0 should be preserved, while v1 should be evicted
        assert!(
            m.resolve_full(&v0).is_some(),
            "v0 should be preserved due to resolve_full promotion"
        );
        assert!(
            m.resolve_full(&v1).is_none(),
            "v1 should be evicted as the oldest unpromoted entry"
        );
    }

    #[test]
    fn test_lru_coherency_adversarial() {
        let m = ContextTokenMap::default();

        let peer0 = "user-adversarial-0";
        let peer1 = "user-adversarial-1";
        let v0 = m.map("real-0".into(), peer0.into(), None);
        let v1 = m.map("real-1".into(), peer1.into(), None);

        // Keep v0 hot by resolving it (uses resolve which only returns real_token,
        // but under the hood it must promote the entire ContextRecord).
        assert!(m.resolve(&v0).is_some());

        // Insert 49,999 other entries to trigger exactly 1 eviction
        for i in 2..=50000 {
            m.map(format!("real-{}", i), format!("user-{}", i), None);
        }

        // Verify that v0 is fully coherent and preserved:
        // 1. resolve_full still returns both real_token and peer_user_id
        let full = m.resolve_full(&v0);
        assert!(
            full.is_some(),
            "v0 should be preserved due to resolve promotion"
        );
        let (_real, peer_id) = full.unwrap();
        assert_eq!(peer_id, peer0, "peer_user_id must be preserved for v0");

        // 2. mapping the same peer with a new real token still returns the same stable v0
        let v_new = m.map("real-new-for-0".into(), peer0.into(), None);
        assert_eq!(
            v_new, v0,
            "conv_to_v mapping must be preserved for stable vctx"
        );

        // Verify that v1 is evicted and not found in any maps
        assert!(m.resolve(&v1).is_none(), "v1 should be evicted");
        assert!(
            m.resolve_full(&v1).is_none(),
            "v1 should be evicted from full resolve"
        );

        // Mapping peer1 with new real token should generate a NEW vtoken since it was evicted
        let v1_new = m.map("real-new-for-1".into(), peer1.into(), None);
        assert_ne!(v1_new, v1, "v1's conversation key should have been evicted");
    }
}

// ─── Client queue ─────────────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct ClientQueue {
    pub(crate) pending: VecDeque<WeixinMessage>,
    pub(crate) notify: Arc<Notify>,
}

#[allow(dead_code)]
impl ClientQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn push(&mut self, msg: WeixinMessage) -> bool {
        let dropped = if self.pending.len() >= MAX_QUEUE_SIZE {
            self.pending.pop_front();
            warn!(
                max = MAX_QUEUE_SIZE,
                "client queue full, dropping oldest message"
            );
            true
        } else {
            false
        };
        self.pending.push_back(msg);
        self.notify.notify_one();
        dropped
    }

    pub(crate) fn drain(&mut self) -> Vec<WeixinMessage> {
        self.pending.drain(..).collect()
    }
}

impl Default for ClientQueue {
    fn default() -> Self {
        Self::new()
    }
}

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

    #[test]
    fn test_mutex_poison_safe() {
        use std::thread;

        // 1. Test ContextTokenMap poison safety
        let m = Arc::new(ContextTokenMap::default());
        let m_clone = m.clone();
        let handle = thread::spawn(move || {
            let _v = m_clone.map("real-ctx".into(), "user1".into(), None);
            panic!("force panic to poison ContextTokenMap Mutex");
        });
        let _ = handle.join();
        // Should not panic on subsequent calls
        let v = m.map("real-ctx-2".into(), "user2".into(), None);
        assert!(!v.is_empty());

        // 2. Test InMemoryQueue (PerClientSlot) poison safety
        let slot = Arc::new(PerClientSlot::new(10));
        let slot_clone = slot.clone();
        let handle3 = thread::spawn(move || {
            let _lock = slot_clone.messages.lock().unwrap();
            panic!("force panic to poison PerClientSlot Mutex");
        });
        let _ = handle3.join();
        // Now test push/drain on the poisoned slot should not panic
        assert!(!slot.push(WeixinMessage::default()));
        assert_eq!(slot.len(), 1);
        assert_eq!(slot.drain().len(), 1);
    }
}
