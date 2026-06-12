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

/// Maximum number of messages buffered per client.
const MAX_QUEUE_SIZE: usize = 200;

/// Maximum number of virtual context token mappings held in memory.
/// Oldest entries are evicted when the limit is reached (LRU policy).
const MAX_CTX_MAP_ENTRIES: usize = 50_000;

// ─── context_token mapping ────────────────────────────────────────────────────

/// Maps virtual context tokens (issued to clients) to real context tokens
/// (from actual iLink upstream). Clients never see the real tokens.
/// Also stores the peer_user_id (WeChat sender) so the hub can set `to_user_id` on replies.
///
/// WeChat may issue a **new** `real_ctx` on every inbound message even in the same DM.
/// `conv_to_v` keeps one stable virtual token per conversation so backend session IDs
/// (e.g. Claude `--resume`) survive across messages.
///
/// All four maps are capped at [`MAX_CTX_MAP_ENTRIES`] with LRU eviction to bound
/// memory usage during long-running deployments.
pub struct ContextTokenMap {
    v_to_real: LruCache<String, String>,
    real_to_v: LruCache<String, String>,
    v_to_peer: LruCache<String, String>,
    /// Stable vctx per conversation (`peer:<id>` or `group:<id>`).
    conv_to_v: LruCache<String, String>,
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
            v_to_real: LruCache::new(cap),
            real_to_v: LruCache::new(cap),
            v_to_peer: LruCache::new(cap),
            conv_to_v: LruCache::new(cap),
        }
    }

    pub fn has_conversation(&self, conv_key: &str) -> bool {
        self.conv_to_v.contains(conv_key)
    }

    /// Seed a known conversation → vctx mapping (e.g. after DB warm-up on hub restart).
    pub fn seed_conversation(
        &mut self,
        conv_key: String,
        vctx: String,
        real_ctx: String,
        peer_user_id: String,
    ) {
        self.conv_to_v.put(conv_key, vctx.clone());
        self.seed_full(vctx, real_ctx, peer_user_id);
    }

    pub fn map(
        &mut self,
        real_token: String,
        peer_user_id: String,
        group_id: Option<&str>,
    ) -> String {
        self.map_scoped(real_token, peer_user_id, group_id, None)
    }

    /// Like `map`, but scopes the stable vctx to a specific client (`client_scope`).
    /// Used in broadcast so each backend gets its own independent vctx.
    pub fn map_scoped(
        &mut self,
        real_token: String,
        peer_user_id: String,
        group_id: Option<&str>,
        client_scope: Option<&str>,
    ) -> String {
        let conv_key = conversation_key(&peer_user_id, group_id).map(|k| match client_scope {
            Some(scope) => format!("{k}@{scope}"),
            None => k,
        });

        if let Some(ref key) = conv_key {
            if let Some(vtoken) = self.conv_to_v.get(key).cloned() {
                self.v_to_real.put(vtoken.clone(), real_token.clone());
                self.real_to_v.put(real_token, vtoken.clone());
                if !peer_user_id.is_empty() {
                    self.v_to_peer.put(vtoken.clone(), peer_user_id);
                }
                return vtoken;
            }
        }

        // For unscoped calls, also check the real_to_v index.
        if client_scope.is_none() {
            if let Some(vtoken) = self.real_to_v.get(&real_token).cloned() {
                if !peer_user_id.is_empty() {
                    self.v_to_peer.put(vtoken.clone(), peer_user_id);
                }
                return vtoken;
            }
        }

        let vtoken = format!("vctx_{}", Uuid::new_v4().simple());
        self.v_to_real.put(vtoken.clone(), real_token.clone());
        self.real_to_v.put(real_token, vtoken.clone());
        if let Some(key) = conv_key {
            self.conv_to_v.put(key, vtoken.clone());
        }
        if !peer_user_id.is_empty() {
            self.v_to_peer.put(vtoken.clone(), peer_user_id);
        }
        vtoken
    }

    /// Number of virtual context token entries currently held in memory.
    pub fn len(&self) -> usize {
        self.v_to_real.len()
    }

    /// Whether the map currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.v_to_real.is_empty()
    }

    pub fn resolve(&mut self, vtoken: &str) -> Option<&str> {
        self.v_to_real.get(vtoken).map(String::as_str)
    }

    /// Returns `(real_ctx, peer_user_id)` for the given virtual token.
    /// Uses `peek` (no LRU promotion) to allow `&self` and read-lock access.
    pub fn resolve_full(&self, vtoken: &str) -> Option<(&str, &str)> {
        let real = self.v_to_real.peek(vtoken)?.as_str();
        let peer = self
            .v_to_peer
            .peek(vtoken)
            .map(String::as_str)
            .unwrap_or("");
        Some((real, peer))
    }

    /// Seed a known mapping into the in-memory cache (without peer_user_id).
    pub fn seed(&mut self, vctx: String, real_ctx: String) {
        self.v_to_real
            .get_or_insert(vctx.clone(), || real_ctx.clone());
        self.real_to_v.get_or_insert(real_ctx, || vctx);
    }

    /// Seed a known mapping including peer_user_id.
    pub fn seed_full(&mut self, vctx: String, real_ctx: String, peer_user_id: String) {
        self.v_to_real
            .get_or_insert(vctx.clone(), || real_ctx.clone());
        self.real_to_v.get_or_insert(real_ctx, || vctx.clone());
        if !peer_user_id.is_empty() {
            self.v_to_peer.get_or_insert(vctx, || peer_user_id);
        }
    }
}

#[cfg(test)]
mod context_map_tests {
    use super::ContextTokenMap;

    #[test]
    fn map_creates_stable_virtual_for_same_real() {
        let mut m = ContextTokenMap::default();
        let v1 = m.map("real-ctx".into(), "user1".into(), None);
        let v2 = m.map("real-ctx".into(), "user1".into(), None);
        assert_eq!(v1, v2);
        assert_eq!(m.resolve(&v1), Some("real-ctx"));
        assert_eq!(m.resolve_full(&v1), Some(("real-ctx", "user1")));
    }

    #[test]
    fn map_reuses_vctx_for_same_peer_even_when_real_ctx_changes() {
        let mut m = ContextTokenMap::default();
        let peer = "user@im.wechat";
        let v1 = m.map("ctx-msg-1".into(), peer.into(), None);
        let v2 = m.map("ctx-msg-2".into(), peer.into(), None);
        assert_eq!(v1, v2);
        assert_eq!(m.resolve(&v2), Some("ctx-msg-2"));
    }

    #[test]
    fn map_different_real_gets_different_virtual() {
        let mut m = ContextTokenMap::default();
        let va = m.map("ctx-a".into(), "u".into(), None);
        let vb = m.map("ctx-b".into(), "v".into(), None);
        assert_ne!(va, vb);
    }

    #[test]
    fn seed_full_then_resolve() {
        let mut m = ContextTokenMap::default();
        m.seed_full("vctx_1".into(), "real".into(), "peer@x".into());
        assert_eq!(m.resolve("vctx_1"), Some("real"));
        assert_eq!(m.resolve_full("vctx_1"), Some(("real", "peer@x")));
    }
}

// ─── Client queue ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ClientQueue {
    pub pending: VecDeque<WeixinMessage>,
    pub notify: Arc<Notify>,
}

impl ClientQueue {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn push(&mut self, msg: WeixinMessage) -> bool {
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

    pub fn drain(&mut self) -> Vec<WeixinMessage> {
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
}

impl PerClientSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            messages: std::sync::Mutex::new(VecDeque::new()),
            notify: Arc::new(Notify::new()),
        })
    }

    fn push(&self, msg: WeixinMessage) -> bool {
        let mut q = self.messages.lock().unwrap();
        let dropped = if q.len() >= MAX_QUEUE_SIZE {
            q.pop_front();
            warn!(
                max = MAX_QUEUE_SIZE,
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
        self.messages.lock().unwrap().drain(..).collect()
    }

    fn len(&self) -> usize {
        self.messages.lock().unwrap().len()
    }
}

pub struct InMemoryQueue {
    slots: DashMap<String, Arc<PerClientSlot>>,
}

impl InMemoryQueue {
    pub fn new() -> Self {
        Self {
            slots: DashMap::new(),
        }
    }

    fn get_or_create(&self, vtoken: &str) -> Arc<PerClientSlot> {
        self.slots
            .entry(vtoken.to_string())
            .or_insert_with(PerClientSlot::new)
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
