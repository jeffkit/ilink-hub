//! Shared Hub state: metrics, long-poll tracking, and the composed [`HubState`]
//! with its `IlinkConnState` / `RoutingState` / `ClientState` sub-states.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use dashmap::DashMap;

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

/// Maximum number of concurrent `getupdates` long-polls allowed Hub-wide,
/// across all vtokens. Each long-poll holds an idle await, a DashMap shard
/// entry, and a `mpsc`/`Notify` channel; 8192 well-behaved clients are
/// already a lot, and any number beyond that strongly suggests either
/// runaway retry storms or a misbehaving bridge. The cap is enforced
/// *before* the per-vtoken gate so a single misconfigured vtoken cannot
/// starve the rest of the fleet.
pub const MAX_HUB_POLLS_DEFAULT: usize = 8192;

// ─── Metrics ──────────────────────────────────────────────────────────────────

/// Bucketed-latency histogram. Uses a fixed log-scale bucket layout to
/// answer "what is the P50 / P95 / P99 of X" without pulling in the full
/// `prometheus-client` crate (which would add a transitive dependency tree
/// of its own). The bucket layout is chosen for sub-second HTTP and CLI
/// latencies; for sub-millisecond paths it's coarse, for multi-second
/// paths it covers the long tail.
///
/// Bucket boundaries in milliseconds. The layout is `[1, 5, 25, 100, 500, 2_500, 10_000, +Inf]`
/// — 8 buckets plus overflow. Suitable for the metrics we currently
/// care about (HTTP round-trips, upstream long-poll cadence, CLI exec).
pub const HISTOGRAM_BUCKETS_MS: &[u64] = &[1, 5, 25, 100, 500, 2_500, 10_000];

/// Number of histogram buckets: one per boundary plus the overflow (+Inf) bucket.
pub const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_BUCKETS_MS.len() + 1;

/// Latency histogram observation. One per metric name; `observe` is hot
/// path (single fetch_add per bucket, no allocation), `snapshot` is the
/// Prometheus-scrape path (called every 15s, not hot).
#[derive(Debug)]
pub struct LatencyHistogram {
    /// Cumulative count of observations.
    pub count: AtomicU64,
    /// Sum of all observed latencies in milliseconds, stored as a plain u64 integer.
    pub sum_ms: AtomicU64,
    /// Fixed-size bucket array: one slot per boundary in `HISTOGRAM_BUCKETS_MS` plus
    /// one overflow (+Inf) slot. Using a fixed-size array avoids heap allocation and
    /// eliminates bounds-check indirection on the hot observe() path.
    pub buckets: [AtomicU64; HISTOGRAM_BUCKET_COUNT],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            // AtomicU64 is not Copy so we cannot use array-repeat syntax; initialize
            // each element explicitly. HISTOGRAM_BUCKET_COUNT is a compile-time constant.
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Record a single observation in milliseconds. `ms` is saturated to
    /// `u64::MAX` if the caller passes a negative value (a clock skew);
    /// we do not panic.
    pub fn observe(&self, ms: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(ms, Ordering::Relaxed);
        // Linear scan is O(8) — fine for our small fixed layout.
        for (i, boundary) in HISTOGRAM_BUCKETS_MS.iter().enumerate() {
            if ms <= *boundary {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // Overflow bucket.
        self.buckets[HISTOGRAM_BUCKETS_MS.len()].fetch_add(1, Ordering::Relaxed);
    }
}

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
    /// Always zero — retained for Prometheus metric continuity after the broadcast→mpsc migration.
    pub dispatcher_lagged: AtomicU64,
    /// Number of message history persist tasks dropped because the semaphore was exhausted.
    /// A non-zero value indicates the DB write pool is too slow for the current message rate.
    pub messages_persist_dropped: AtomicU64,
    /// Latency of `getupdates` long-polls, measured from handler entry to
    /// handler exit (covers both the wakeup wait and the drain).
    pub getupdates_latency_ms: LatencyHistogram,
    /// Latency of upstream `sendmessage` HTTP round-trips. Excludes Hub
    /// internal bookkeeping (context translation, footer append, etc.).
    pub sendmessage_upstream_latency_ms: LatencyHistogram,
    /// Latency of the inbound dispatch pipeline (in-memory only — no DB
    /// I/O). Excludes `tokio::spawn` wall-clock time; this is the
    /// synchronous time from `dispatch_message` entry to its first
    /// `push_to_queue` call.
    pub dispatch_latency_ms: LatencyHistogram,
    /// Unix timestamp (seconds) when this `Metrics` instance was created — emitted as
    /// `*_created` in Prometheus text format so scrape tools can compute per-second rates
    /// correctly even after a process restart.
    pub process_start_unix_secs: f64,
}

impl Metrics {
    pub fn new() -> Self {
        let process_start_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        Self {
            messages_dispatched: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            upstream_user_messages: AtomicU64::new(0),
            sendmessage_total: AtomicU64::new(0),
            sendmessage_errors: AtomicU64::new(0),
            relogin_attempts: AtomicU64::new(0),
            dispatcher_lagged: AtomicU64::new(0),
            messages_persist_dropped: AtomicU64::new(0),
            getupdates_latency_ms: LatencyHistogram::new(),
            sendmessage_upstream_latency_ms: LatencyHistogram::new(),
            dispatch_latency_ms: LatencyHistogram::new(),
            process_start_unix_secs,
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that records elapsed time into a `LatencyHistogram` on drop.
/// Shared between the `getupdates` handler (`HistoGuard` alias in routes.rs)
/// and the inbound dispatch pipeline (`dispatch.rs`).
pub struct LatencyGuard<'a> {
    start: std::time::Instant,
    histogram: &'a LatencyHistogram,
}

impl<'a> LatencyGuard<'a> {
    pub fn new(histogram: &'a LatencyHistogram) -> Self {
        Self { start: std::time::Instant::now(), histogram }
    }
}

impl Drop for LatencyGuard<'_> {
    fn drop(&mut self) {
        self.histogram.observe(self.start.elapsed().as_millis() as u64);
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
#[derive(Debug, Default)]
pub struct PollTracker {
    /// Per-vtoken concurrent poll counter. Public for test-only access so
    /// integration tests can poison the mutex to verify the let-Ok
    /// panic-safety path (F-M2-2); production code should only call
    /// `enter` / rely on `Drop`.
    pub counts: StdMutex<HashMap<String, usize>>,
    /// Hub-wide total of in-flight long-polls. Guarded by an AtomicUsize
    /// (lock-free fast path) so the per-request cost of the global gate is
    /// a single fetch_add — orders of magnitude cheaper than the
    /// per-vtoken `StdMutex` we already pay.
    total: AtomicUsize,
    /// Operator-configured Hub-wide cap. Defaults to [`MAX_HUB_POLLS_DEFAULT`].
    hub_cap: AtomicUsize,
}

impl PollTracker {
    /// Set the Hub-wide cap. Must be called once at startup before the Hub
    /// serves any traffic; subsequent changes are not thread-safe w.r.t.
    /// in-flight `enter` calls (they observe either the old or the new cap).
    pub fn set_hub_cap(&self, cap: usize) {
        self.hub_cap.store(cap, Ordering::Relaxed);
    }

    /// Current Hub-wide total of in-flight polls. For metrics / tests.
    pub fn total_polls(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Register a new active poll for `vtoken`. Returns the per-vtoken count
    /// (always >= 1 on the success path, 0 when the per-vtoken mutex is poisoned)
    /// and a guard that decrements *both* the per-vtoken count and the Hub-wide
    /// total on drop.
    ///
    /// The Hub-wide gate runs *before* the per-vtoken gate so a single
    /// misbehaving vtoken cannot starve the rest of the fleet: even if all
    /// per-vtoken slots are full, the Hub still serves polls from other vtokens
    /// up to the global cap.
    ///
    /// F-M2-2: never panic on mutex poisoning. If the per-vtoken `counts` mutex
    /// is poisoned, the guard is still produced but the count is reported as
    /// 0 (so the per-vtoken 429 gate won't trip) and the drop handler becomes
    /// a best-effort no-op. A poisoned `counts` map is a process-wide bug, but
    /// it must not take the Tokio worker down on every subsequent long-poll.
    /// The Hub-wide counter is lock-free AtomicUsize, so it cannot poison.
    pub fn enter(self: &Arc<Self>, vtoken: &str) -> EnterOutcome {
        // Hub-wide gate first. fetch_add returns the previous value; we then
        // check the new total against the cap. If we're over, decrement back
        // and refuse. The decrement is safe because we just incremented — the
        // counter cannot have wrapped in between on any platform usize can
        // represent.
        let cap = self.hub_cap.load(Ordering::Relaxed);
        let prev_total = self.total.fetch_add(1, Ordering::AcqRel);
        if prev_total >= cap {
            self.total.fetch_sub(1, Ordering::AcqRel);
            return EnterOutcome::HubLimitReached {
                total: prev_total,
                cap,
            };
        }

        let count = {
            let Ok(mut counts) = self.counts.lock() else {
                // Do NOT roll back the Hub-wide increment here. The guard
                // we return is responsible for decrementing `total` on drop,
                // keeping the counter accurate for the duration of the
                // (poisoned) request. Rolling back here and then letting the
                // guard decrement again would cause an underflow.
                return EnterOutcome::Poisoned {
                    guard: PollGuard {
                        tracker: Arc::clone(self),
                        vtoken: vtoken.to_string(),
                    },
                };
            };
            let c = counts.entry(vtoken.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        EnterOutcome::Ok {
            per_vtoken: count,
            guard: PollGuard {
                tracker: Arc::clone(self),
                vtoken: vtoken.to_string(),
            },
        }
    }
}

/// Result of [`PollTracker::enter`]. The caller inspects the variant and
/// either serves the long-poll (Ok), rejects it as 503 (HubLimitReached),
/// or treats the per-vtoken count as advisory and serves anyway (Poisoned).
#[derive(Debug)]
pub enum EnterOutcome {
    Ok { per_vtoken: usize, guard: PollGuard },
    HubLimitReached { total: usize, cap: usize },
    Poisoned { guard: PollGuard },
}

impl EnterOutcome {
    /// Convenience: extract the guard, regardless of variant. `HubLimitReached`
    /// has no guard (the Hub-wide increment was rolled back); callers must
    /// surface the rejection *before* calling this.
    #[allow(dead_code)]
    pub fn guard(self) -> Option<PollGuard> {
        match self {
            EnterOutcome::Ok { guard, .. } | EnterOutcome::Poisoned { guard } => Some(guard),
            EnterOutcome::HubLimitReached { .. } => None,
        }
    }
}

/// RAII guard returned by [`PollTracker::enter`]; decrements the per-vtoken poll count
/// and the Hub-wide total when the long-poll handler returns (success, timeout, shutdown,
/// or client disconnect).
#[derive(Debug)]
pub struct PollGuard {
    tracker: Arc<PollTracker>,
    vtoken: String,
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        // F-M2-2: best-effort decrement; a poisoned mutex here would otherwise
        // propagate a panic into the Tokio worker that called the handler.
        // The Hub-wide counter cannot be poisoned (it's an AtomicUsize), so
        // we always decrement it. saturating_sub guards against the
        // (theoretically impossible) underflow.
        self.tracker.total.fetch_sub(1, Ordering::AcqRel);
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
    /// Lock-free last-seen timestamps (Unix seconds) per vtoken.
    /// `getupdates` updates this atomically without acquiring the registry write lock.
    /// `spawn_health_checker` reads it to mark clients offline.
    pub last_seen: Arc<DashMap<String, AtomicU64>>,
}

impl ClientState {
    pub(crate) fn new(queue: Arc<dyn MessageQueue>) -> Self {
        let poll_tracker = Arc::new(PollTracker::default());
        // Initialize the Hub-wide cap to the default. Operators can override
        // via `ILINK_MAX_HUB_POLLS`; see [`crate::runtime::serve::RuntimeConfig`].
        poll_tracker.set_hub_cap(MAX_HUB_POLLS_DEFAULT);
        Self {
            registry: RwLock::new(ClientRegistry::new()),
            pairing: RwLock::new(PairingRegistry::new()),
            pairing_notify: Arc::new(tokio::sync::Notify::new()),
            queue,
            poll_tracker,
            last_seen: Arc::new(DashMap::new()),
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
/// Admin authentication configuration, parsed once at startup from env vars and
/// stored in `HubState`. Routes call `state.admin.check(headers)` instead of
/// reading env vars at request time via `OnceLock`.
#[derive(Debug, Clone)]
pub struct AdminConfig {
    /// The required Bearer token value, or `None` if `ILINK_ADMIN_TOKEN` is unset.
    pub token: Option<String>,
    /// True when `ILINK_ADMIN_INSECURE_NO_AUTH` is set and no token is configured.
    pub insecure_no_auth: bool,
    /// Parsed once at startup from `ILINKHUB_OUTBOUND_ORIGIN_LABEL`.
    /// `None` = env var absent (default behaviour); `Some(v)` = explicit override.
    pub outbound_origin_label: Option<String>,
}

impl AdminConfig {
    pub fn from_env() -> Self {
        let token = std::env::var("ILINK_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let insecure_no_auth = token.is_none()
            && std::env::var("ILINK_ADMIN_INSECURE_NO_AUTH")
                .ok()
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
        let outbound_origin_label = std::env::var("ILINKHUB_OUTBOUND_ORIGIN_LABEL").ok();
        Self {
            token,
            insecure_no_auth,
            outbound_origin_label,
        }
    }
}

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
    /// Admin authentication config, parsed once at startup.
    pub admin: AdminConfig,
}

impl HubState {
    /// Build a new [`HubState`]. The `relay_secret` must be supplied by the
    /// caller; use [`crate::paths::load_or_create_relay_secret`] for the
    /// standard "load from disk, else generate and persist" path. We pass
    /// it in rather than computing it here so the constructor stays sync
    /// (callers from async contexts can `await` the I/O themselves) and so
    /// tests can pin a deterministic value.
    pub fn new(
        upstream: Arc<dyn UpstreamSink>,
        store: Arc<Store>,
        queue: Arc<dyn MessageQueue>,
        shutdown: watch::Receiver<bool>,
        relay_secret: String,
        admin: AdminConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            ilink: IlinkConnState::new(upstream, shutdown),
            routing: RoutingState::new(),
            clients: ClientState::new(queue),
            store,
            metrics: Arc::new(Metrics::new()),
            persist_sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PERSIST_TASKS)),
            relay_secret,
            admin,
        })
    }
}
