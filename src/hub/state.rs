//! Shared Hub state: metrics, long-poll tracking, and the composed [`HubState`]
//! with its `IlinkConnState` / `RoutingState` / `ClientState` sub-states.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, watch, Mutex, RwLock};

use crate::ilink::{QrLoginUiEvent, UpstreamSink};
use crate::store::Store;

// Hub-internal re-exports (Router, QuoteRouteIndex, ClientRegistry, PairingRegistry,
// MessageQueue) and the `ilink_status` module come from the crate's `hub` module.
use super::*;

// ─── Concurrency limits ───────────────────────────────────────────────────────

/// Maximum number of concurrent `getupdates` long-polls allowed for a single vtoken.
///
/// A healthy backend has exactly one bridge process polling its vtoken at a time.
/// When two or more bridge processes share one credential/token, they race for
/// the destructive `drain` of the per-vtoken message queue and inbound messages
/// get stolen non-deterministically (split-brain). To stop a malicious or
/// misconfigured client from holding an unbounded number of long-polls (which
/// would saturate the Tokio worker pool), the Hub caps the concurrent poll
/// count per vtoken at this value and rejects additional polls with HTTP 429.
///
/// SEC-003: a single vtoken must not be able to exhaust Hub resources. The
/// cap is intentionally small — anything beyond ~3 is already a configuration
/// problem worth surfacing in the operator logs.
pub const MAX_CONCURRENT_POLLS_PER_VTOKEN: usize = 3;

// ─── Metrics ──────────────────────────────────────────────────────────────────

pub struct Metrics {
    pub messages_dispatched: AtomicU64,
    pub messages_dropped: AtomicU64,
    /// User-side (or command) messages taken from upstream and passed into routing
    /// (excludes bot-side echo copies with `message_type == 2`).
    pub upstream_user_messages: AtomicU64,
    /// Total sendmessage calls from backend clients.
    pub sendmessage_total: AtomicU64,
    /// sendmessage calls that were rejected (unknown token, missing context, etc.).
    pub sendmessage_errors: AtomicU64,
    /// Number of QR re-login attempts triggered (manual or automatic).
    pub relogin_attempts: AtomicU64,
    /// Number of messages missed because the dispatcher lagged behind the broadcast channel.
    pub dispatcher_lagged: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_dispatched: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            upstream_user_messages: AtomicU64::new(0),
            sendmessage_total: AtomicU64::new(0),
            sendmessage_errors: AtomicU64::new(0),
            relogin_attempts: AtomicU64::new(0),
            dispatcher_lagged: AtomicU64::new(0),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Concurrent long-poll tracker ─────────────────────────────────────────────

/// Tracks how many `getupdates` long-polls are concurrently active per vtoken.
///
/// A healthy backend has at most one process polling its vtoken at a time. Two or more
/// concurrent polls for the same vtoken mean multiple bridge processes share one
/// credential/token and are competing for the same per-vtoken message queue (`drain` is a
/// destructive read), so inbound messages get stolen non-deterministically. This tracker
/// lets the Hub surface that misconfiguration instead of failing silently.
#[derive(Default)]
pub struct PollTracker {
    /// Per-vtoken concurrent poll counter. Public for test-only access so
    /// integration tests can poison the mutex to verify the let-Ok
    /// panic-safety path (F-M2-2); production code should only call
    /// `enter` / rely on `Drop`.
    pub counts: StdMutex<HashMap<String, usize>>,
}

impl PollTracker {
    /// Register a new active poll for `vtoken`. Returns the number of polls now concurrently
    /// active for that vtoken (always >= 1) and a guard that decrements the count on drop.
    ///
    /// F-M2-2: never panic on mutex poisoning. If the counts mutex is poisoned, the
    /// guard is still produced but the count is reported as 0 (which means the 429
    /// gate won't trip on this vtoken) and the drop handler becomes a best-effort
    /// no-op. A poisoned `counts` map is a process-wide bug, but it must not take
    /// the Tokio worker down on every subsequent long-poll.
    pub fn enter(self: &Arc<Self>, vtoken: &str) -> (usize, PollGuard) {
        let count = {
            let Ok(mut counts) = self.counts.lock() else {
                return (
                    0,
                    PollGuard {
                        tracker: Arc::clone(self),
                        vtoken: vtoken.to_string(),
                    },
                );
            };
            let c = counts.entry(vtoken.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        (
            count,
            PollGuard {
                tracker: Arc::clone(self),
                vtoken: vtoken.to_string(),
            },
        )
    }
}

/// RAII guard returned by [`PollTracker::enter`]; decrements the per-vtoken poll count when
/// the long-poll handler returns (success, timeout, shutdown, or client disconnect).
pub struct PollGuard {
    tracker: Arc<PollTracker>,
    vtoken: String,
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        // F-M2-2: best-effort decrement; a poisoned mutex here would otherwise
        // propagate a panic into the Tokio worker that called the handler.
        let Ok(mut counts) = self.tracker.counts.lock() else {
            return;
        };
        if let Some(c) = counts.get_mut(&self.vtoken) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                counts.remove(&self.vtoken);
            }
        }
    }
}

// ─── Shared Hub State ─────────────────────────────────────────────────────────

/// State tied to the iLink upstream WebSocket connection.
///
/// Anything that mutates only when iLink connects, logs in, or sends a QR-ready
/// event lives here. Callers that need to send a message upstream, observe a QR
/// login, or trigger a re-login take a reference to this sub-state rather than
/// touching the whole `HubState`.
///
/// `upstream` is held as a trait object so end-to-end tests can inject a
/// recording mock in place of [`UpstreamClient`]. The polling loop owns the
/// concrete `UpstreamClient` separately and does not go through this field.
/// The observability counters on the polling loop are exposed through the
/// `UpstreamSink::polls_ok` / `polls_err` / `relogin_attempts` accessors.
pub struct IlinkConnState {
    pub upstream: Arc<dyn UpstreamSink>,
    /// Shared with Axum graceful shutdown; long-poll handlers exit early when this becomes `true`.
    pub shutdown: watch::Receiver<bool>,
    /// Current iLink upstream status (see [`ilink_status`] constants).
    pub ilink_status: Arc<AtomicU8>,
    /// Broadcasts QR login UI events to SSE subscribers.
    pub qr_tx: broadcast::Sender<QrLoginUiEvent>,
    /// Last QR Ready event — replayed to new SSE subscribers that connect after it was sent.
    pub qr_last_ready: Arc<Mutex<Option<QrLoginUiEvent>>>,
    /// Signals the polling loop to initiate a fresh QR re-login.
    pub relogin_tx: broadcast::Sender<()>,
    /// Single-use, short-lived tickets that authenticate the QR SSE stream
    /// without putting the admin token in the URL. See [`SseTicketStore`].
    pub qr_ticket: crate::server::sse_ticket::SseTicketStore,
}

impl IlinkConnState {
    pub(crate) fn new(upstream: Arc<dyn UpstreamSink>, shutdown: watch::Receiver<bool>) -> Self {
        let (qr_tx, _) = broadcast::channel(16);
        let (relogin_tx, _) = broadcast::channel(4);
        Self {
            upstream,
            shutdown,
            ilink_status: Arc::new(AtomicU8::new(ilink_status::UNKNOWN)),
            qr_tx,
            qr_last_ready: Arc::new(Mutex::new(None)),
            relogin_tx,
            qr_ticket: crate::server::sse_ticket::SseTicketStore::new(),
        }
    }
}

/// Routing-layer state: per-message dispatch decisions, conversation vctx
/// mapping, and quote-reply tracking. Pure in-memory; no I/O.
pub struct RoutingState {
    pub router: Mutex<Router>,
    /// Quote-reply → backend / hub command (see [`quote_route`]).
    pub quote_index: Mutex<QuoteRouteIndex>,
}

impl RoutingState {
    pub(crate) fn new() -> Self {
        Self {
            router: Mutex::new(Router::new(None)),
            quote_index: Mutex::new(QuoteRouteIndex::default()),
        }
    }
}

/// Registered backend clients, paired devices, the per-vtoken message queue,
/// and long-poll concurrency tracking.
pub struct ClientState {
    pub registry: RwLock<ClientRegistry>,
    pub pairing: RwLock<PairingRegistry>,
    /// Notified whenever a pairing session transitions state (scanned/confirmed).
    /// `get_qrcode_status` waits on this instead of sleep-polling every 1s.
    pub pairing_notify: Arc<tokio::sync::Notify>,
    pub queue: Arc<dyn MessageQueue>,
    /// Tracks concurrent `getupdates` long-polls per vtoken to detect bridges that share one
    /// credential/token (queue split-brain).
    pub poll_tracker: Arc<PollTracker>,
}

impl ClientState {
    pub(crate) fn new(queue: Arc<dyn MessageQueue>) -> Self {
        Self {
            registry: RwLock::new(ClientRegistry::new()),
            pairing: RwLock::new(PairingRegistry::new()),
            pairing_notify: Arc::new(tokio::sync::Notify::new()),
            queue,
            poll_tracker: Arc::new(PollTracker::default()),
        }
    }
}

/// Maximum number of concurrent fire-and-forget persist tasks. Applying this limit
/// bounds the number of SQLite pool-acquire waiters during message bursts and
/// prevents them from growing without bound. Tasks that cannot acquire a permit
/// drop their work and increment the relevant failure counter — the same observable
/// behaviour as before, but now with natural backpressure.
const MAX_CONCURRENT_PERSIST_TASKS: usize = 32;

/// Top-level hub state. Groups related state into cohesive sub-states so that
/// internal helpers (dispatcher, hub-command handler, etc.) take the smallest
/// slice they need instead of the entire blob.
///
/// External callers (server routes, pairing, etc.) continue to access fields
/// through the same `state.field` paths they always have — the sub-state
/// fields are re-exported as direct `pub` fields on `HubState` for backward
/// compatibility. New code is encouraged to take `&RoutingState` /
/// `&IlinkConnState` / `&ClientState` parameters to make the dependency
/// explicit.
pub struct HubState {
    /// iLink upstream connection and shutdown signal.
    pub ilink: IlinkConnState,
    /// Per-message routing, vctx mapping, and quote-reply tracking.
    pub routing: RoutingState,
    /// Registered clients, paired devices, message queue, long-poll tracking.
    pub clients: ClientState,
    /// Persistent store (SQLx pool-backed). Cross-cutting; not part of any sub-state.
    pub store: Arc<Store>,
    /// Observability counters. Cross-cutting; not part of any sub-state.
    pub metrics: Arc<Metrics>,
    /// Backpressure semaphore for fire-and-forget context-token persist tasks.
    pub persist_sem: Arc<tokio::sync::Semaphore>,
    /// Per-process random secret shared with the in-process relay client so the Hub
    /// can distinguish trusted relay-forwarded XFF headers from local-process spoofing.
    /// The relay client injects `X-Ilink-Relay-Secret: <secret>` on every forwarded
    /// request; Hub's pair_confirm trusts X-Forwarded-For only when this matches.
    pub relay_secret: String,
}

impl HubState {
    pub fn new(
        upstream: Arc<dyn UpstreamSink>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        use rand::distributions::Alphanumeric;
        use rand::Rng;
        let relay_secret: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        Arc::new(Self {
            ilink: IlinkConnState::new(upstream, shutdown),
            routing: RoutingState::new(),
            clients: ClientState::new(queue),
            store,
            metrics: Arc::new(Metrics::new()),
            persist_sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PERSIST_TASKS)),
            relay_secret,
        })
    }
}
