//! Per-client message queue — trait-based abstraction with in-memory default.
//!
//! The [`MessageQueue`] trait defines the contract for all queue backends.
//! [`InMemoryQueue`] is the default implementation backed by a `tokio::sync::Mutex`.

use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::warn;
use uuid::Uuid;

use crate::error::HubError;
use crate::ilink::types::WeixinMessage;

/// Maximum number of messages buffered per client.
const MAX_QUEUE_SIZE: usize = 200;

// ─── context_token mapping ────────────────────────────────────────────────────

/// Maps virtual context tokens (issued to clients) to real context tokens
/// (from actual iLink upstream). Clients never see the real tokens.
/// Also stores the peer_user_id (WeChat sender) so the hub can set `to_user_id` on replies.
#[derive(Debug, Default)]
pub struct ContextTokenMap {
    v_to_real: HashMap<String, String>,
    real_to_v: HashMap<String, String>,
    v_to_peer: HashMap<String, String>,
}

impl ContextTokenMap {
    pub fn map(&mut self, real_token: String, peer_user_id: String) -> String {
        if let Some(vtoken) = self.real_to_v.get(&real_token) {
            if !peer_user_id.is_empty() {
                self.v_to_peer.insert(vtoken.clone(), peer_user_id);
            }
            return vtoken.clone();
        }
        let vtoken = format!("vctx_{}", Uuid::new_v4().simple());
        self.v_to_real.insert(vtoken.clone(), real_token.clone());
        self.real_to_v.insert(real_token, vtoken.clone());
        if !peer_user_id.is_empty() {
            self.v_to_peer.insert(vtoken.clone(), peer_user_id);
        }
        vtoken
    }

    pub fn resolve(&self, vtoken: &str) -> Option<&str> {
        self.v_to_real.get(vtoken).map(String::as_str)
    }

    /// Returns `(real_ctx, peer_user_id)` for the given virtual token.
    pub fn resolve_full(&self, vtoken: &str) -> Option<(&str, &str)> {
        let real = self.v_to_real.get(vtoken)?.as_str();
        let peer = self.v_to_peer.get(vtoken).map(String::as_str).unwrap_or("");
        Some((real, peer))
    }

    /// Seed a known mapping into the in-memory cache (without peer_user_id).
    pub fn seed(&mut self, vctx: String, real_ctx: String) {
        self.v_to_real
            .entry(vctx.clone())
            .or_insert_with(|| real_ctx.clone());
        self.real_to_v.entry(real_ctx).or_insert(vctx);
    }

    /// Seed a known mapping including peer_user_id.
    pub fn seed_full(&mut self, vctx: String, real_ctx: String, peer_user_id: String) {
        self.v_to_real
            .entry(vctx.clone())
            .or_insert_with(|| real_ctx.clone());
        self.real_to_v
            .entry(real_ctx)
            .or_insert_with(|| vctx.clone());
        if !peer_user_id.is_empty() {
            self.v_to_peer.entry(vctx).or_insert(peer_user_id);
        }
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

pub struct InMemoryQueue {
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
}

impl InMemoryQueue {
    pub fn new() -> Self {
        Self {
            queues: tokio::sync::Mutex::new(HashMap::new()),
        }
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
        let mut queues = self.queues.lock().await;
        let queue = queues.entry(vtoken.to_string()).or_default();
        let dropped = queue.push(msg);
        Ok(dropped)
    }

    async fn drain(&self, vtoken: &str) -> Result<Vec<WeixinMessage>, HubError> {
        let mut queues = self.queues.lock().await;
        Ok(queues
            .get_mut(vtoken)
            .map(|q| q.drain())
            .unwrap_or_default())
    }

    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError> {
        let notify = {
            let mut queues = self.queues.lock().await;
            queues.entry(vtoken.to_string()).or_default().notify.clone()
        };
        let result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), notify.notified()).await;
        Ok(result.is_ok())
    }

    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError> {
        let mut queues = self.queues.lock().await;
        queues.remove(vtoken);
        Ok(())
    }

    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
        let queues = self.queues.lock().await;
        Ok(queues
            .iter()
            .map(|(k, q)| (k.clone(), q.pending.len()))
            .collect())
    }
}
