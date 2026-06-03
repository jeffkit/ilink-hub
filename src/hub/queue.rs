/// Per-client message queue — buffered channel-based approach.
/// Each registered client gets a queue; when getupdates is called,
/// messages are drained from the front.

use std::collections::{HashMap, VecDeque};
use tokio::sync::Notify;
use std::sync::Arc;
use uuid::Uuid;

use crate::ilink::types::InboundMessage;

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

    pub fn push(&mut self, msg: InboundMessage) {
        self.pending.push_back(msg);
        self.notify.notify_one();
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

// ─── QueueStore ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct QueueStore {
    /// vtoken → queue
    queues: HashMap<String, ClientQueue>,
}

impl QueueStore {
    pub fn new() -> Self {
        Self {
            queues: HashMap::new(),
        }
    }
}

impl Default for QueueStore {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueStore {
    pub fn ensure(&mut self, vtoken: &str) {
        self.queues.entry(vtoken.to_string()).or_default();
    }

    pub fn push(&mut self, vtoken: &str, msg: InboundMessage) {
        let queue = self.queues.entry(vtoken.to_string()).or_default();
        queue.push(msg);
    }

    pub fn drain(&mut self, vtoken: &str) -> Vec<InboundMessage> {
        self.queues
            .get_mut(vtoken)
            .map(|q| q.drain())
            .unwrap_or_default()
    }

    /// Returns the Notify handle for a client's queue (used for long-poll waiting).
    pub fn notify_handle(&self, vtoken: &str) -> Option<Arc<Notify>> {
        self.queues.get(vtoken).map(|q| q.notify.clone())
    }
}
