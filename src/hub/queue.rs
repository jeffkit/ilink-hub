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
use crate::ilink::types::InboundMessage;

/// Maximum number of messages buffered per client.
/// Oldest messages are dropped when the limit is exceeded (client offline too long).
const MAX_QUEUE_SIZE: usize = 200;

// ─── context_token mapping ────────────────────────────────────────────────────

/// Maps virtual context tokens (issued to clients) to real context tokens
/// (from actual iLink upstream). Clients never see the real tokens.
#[derive(Debug, Default)]
pub struct ContextTokenMap {
    /// vctx → real_ctx
    v_to_real: HashMap<String, String>,
    /// real_ctx → vctx (for dedup / lookup)
    real_to_v: HashMap<String, String>,
}

impl ContextTokenMap {
    pub fn map(&mut self, real_token: String) -> String {
        if let Some(vtoken) = self.real_to_v.get(&real_token) {
            return vtoken.clone();
        }
        let vtoken = format!("vctx_{}", Uuid::new_v4().simple());
        self.v_to_real.insert(vtoken.clone(), real_token.clone());
        self.real_to_v.insert(real_token, vtoken.clone());
        vtoken
    }

    pub fn resolve(&self, vtoken: &str) -> Option<&str> {
        self.v_to_real.get(vtoken).map(String::as_str)
    }

    /// Seed a known mapping into the in-memory cache (used on startup / DB fallback warm-up).
    pub fn seed(&mut self, vctx: String, real_ctx: String) {
        self.v_to_real
            .entry(vctx.clone())
            .or_insert_with(|| real_ctx.clone());
        self.real_to_v.entry(real_ctx).or_insert(vctx);
    }
}

// ─── Client queue ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ClientQueue {
    /// Pending messages for this client
    pub pending: VecDeque<InboundMessage>,
    /// Notified when a new message is pushed
    pub notify: Arc<Notify>,
}

impl ClientQueue {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Push a message onto the queue. Returns `true` if the oldest message was
    /// dropped to make room (overflow at [`MAX_QUEUE_SIZE`]).
    pub fn push(&mut self, msg: InboundMessage) -> bool {
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

    /// Drain all pending messages at once (simulates getupdates returning a batch).
    pub fn drain(&mut self) -> Vec<InboundMessage> {
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
/// This trait is object-safe and is intended to be used as `Arc<dyn MessageQueue>`.
/// All async methods are erased via `#[async_trait]` to enable dynamic dispatch.
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
/// use ilink_hub::ilink::types::InboundMessage;
/// use async_trait::async_trait;
/// use std::collections::HashMap;
/// use std::sync::Arc;
///
/// struct CustomQueue;
///
/// #[async_trait]
/// impl MessageQueue for CustomQueue {
///     async fn push(&self, _vtoken: &str, _msg: InboundMessage) -> Result<bool, HubError> {
///         Ok(false)
///     }
///     async fn drain(&self, _vtoken: &str) -> Result<Vec<InboundMessage>, HubError> {
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
///
/// let queue: Arc<dyn MessageQueue> = Arc::new(CustomQueue);
/// // let state = HubState::new(upstream, store, queue);
/// ```
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Push a message onto the queue for the given virtual token.
    ///
    /// Returns `Ok(true)` if the queue was at capacity and the oldest message
    /// was dropped to make room (overflow). Returns `Ok(false)` if the message
    /// was enqueued without eviction. Returns `Err` on backend failure.
    async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<bool, HubError>;

    /// Drain all pending messages for the given virtual token.
    ///
    /// Returns an empty `Vec` if the client has no queued messages.
    async fn drain(&self, vtoken: &str) -> Result<Vec<InboundMessage>, HubError>;

    /// Wait for a notification that a message is available, with a timeout.
    ///
    /// Returns `Ok(true)` if a message notification was received before the
    /// timeout, `Ok(false)` if the timeout expired with no notification.
    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError>;

    /// Remove all state for the given virtual token (client disconnected).
    ///
    /// Calling this for a vtoken that was never registered is a safe no-op.
    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError>;

    /// Returns current queue sizes (vtoken → pending count) for metrics.
    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError>;
}

// ─── InMemoryQueue ────────────────────────────────────────────────────────────

/// Default in-memory queue backend backed by a `tokio::sync::Mutex`.
///
/// All methods use interior mutability — no `&mut self` is needed by callers.
/// Queue entries are created on demand when `push` or `wait_notify` is first
/// called for a vtoken.
pub struct InMemoryQueue {
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
}

impl InMemoryQueue {
    /// Create a new empty in-memory queue.
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
    async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<bool, HubError> {
        let mut queues = self.queues.lock().await;
        let queue = queues.entry(vtoken.to_string()).or_default();
        let dropped = queue.push(msg);
        Ok(dropped)
    }

    async fn drain(&self, vtoken: &str) -> Result<Vec<InboundMessage>, HubError> {
        let mut queues = self.queues.lock().await;
        Ok(queues
            .get_mut(vtoken)
            .map(|q| q.drain())
            .unwrap_or_default())
    }

    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError> {
        // Clone the Arc<Notify> BEFORE releasing the lock so push() can wake us.
        let notify = {
            let mut queues = self.queues.lock().await;
            queues.entry(vtoken.to_string()).or_default().notify.clone()
        };
        // Lock is released here — push() can now acquire it and call notify_one().
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
