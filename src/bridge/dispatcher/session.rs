use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bridge::config::BridgeApp;
use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};

use super::handle::handle_one_message;
use super::send::{sanitize_errmsg, HubClient, SendOutcome};
use super::BridgeStop;

pub(super) fn session_dispatch_key(msg: &WeixinMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default");
    format!("{ctx}:{session_name}")
}

pub(super) async fn run_session_worker(
    key: String,
    mut rx: mpsc::Receiver<WeixinMessage>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
) {
    const SESSION_WORKER_MAX_BACKOFF_SECS: u64 = 60;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Wait for next message; yield immediately if shutdown was already requested.
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            msg_opt = rx.recv() => match msg_opt {
                Some(m) => m,
                None => {
                    info!(session_key = %key, "session worker exiting");
                    return;
                }
            },
        };

        // Capture the routing identifiers needed for an error reply *before* moving `msg`
        // into `handle_one_message`, so that if the future is cancelled we can still send
        // feedback to the user.
        let ctx_for_err = msg.context_token.clone().unwrap_or_default();
        let from_for_err = msg.from_user_id.clone().unwrap_or_default();
        // Capture the session name so the shutdown error reply carries the same session in
        // its ilink_hub_ext. Without this the Hub appends the *current active* session to
        // the footer and registers the error message under that session in the quote_index,
        // causing any quote-reply to that "响应中断" message to be routed to the wrong session.
        let session_name_for_err = msg
            .ilink_hub_ext
            .as_ref()
            .and_then(|e| e.session_name.clone())
            .filter(|s| !s.trim().is_empty());

        let result = tokio::select! {
            biased;
            // Shutdown arrived while we were processing — cancel the AI call and tell user.
            _ = shutdown.cancelled() => {
                if app.send_error_reply() && !ctx_for_err.is_empty() {
                    let mut req = SendMessageRequest::reply(
                        ctx_for_err,
                        "⚠️ 响应中断（服务正在重启），请稍后重发消息".to_string(),
                        &from_for_err,
                    );
                    // Attach the session name so Hub appends the correct session to the
                    // footer and registers the error reply under the right session in the
                    // quote_index. This prevents a quote-reply on this message from being
                    // routed to whichever session happens to be active at shutdown time.
                    if let Some(ref sn) = session_name_for_err {
                        if let Some(ref mut msg) = req.msg {
                            let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                            ext.session_name = Some(sn.clone());
                        }
                    }
                    match client.sendmessage(req).await {
                        Ok(SendOutcome::Sent) => {}
                        Ok(SendOutcome::Throttled { ret, errmsg }) => {
                            // User did not receive the "service restarting,
                            // please resend" notice. This is an
                            // observability hit for the user. M3 must
                            // include this path when wiring buffer+retry.
                            warn!(
                                ret,
                                errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                                "sendmessage throttled during shutdown error reply; user did NOT receive restart notice — M3 must cover this path when adding buffer+retry"
                            );
                        }
                        Err(e) => warn!(error = %e, "failed to send shutdown error reply"),
                    }
                }
                return;
            }
            r = handle_one_message(&client, &app, msg, shutdown.clone()) => r,
        };

        match result {
            Ok(()) => {
                consecutive_failures = 0;
            }
            Err(HandleError::Fatal(reason)) => {
                error!(session_key = %key, reason = ?reason, "fatal CLI error; signalling bridge stop");
                let _ = stop_tx.send(Some(reason));
                return;
            }
            Err(HandleError::Transient(e)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff_secs =
                    SESSION_WORKER_MAX_BACKOFF_SECS.min(1_u64 << consecutive_failures.min(63));
                error!(
                    session_key = %key,
                    error = %e,
                    consecutive_failures,
                    backoff_secs,
                    "message handler failed; backing off before next message"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
        }
    }
}

pub(super) enum HandleError {
    Transient(anyhow::Error),
    Fatal(BridgeStop),
}

impl From<anyhow::Error> for HandleError {
    fn from(e: anyhow::Error) -> Self {
        HandleError::Transient(e)
    }
}

const DEFAULT_SESSION_QUEUE_SIZE: usize = 200;
/// Maximum number of concurrent session workers. Each worker holds an open mpsc channel and a
/// spawned Tokio task; unbounded growth would exhaust both. When the cap is reached, the oldest
/// idle (closed-channel) entries are evicted first; if all entries are still active the new
/// message is dropped with a warning.
const MAX_SESSION_WORKERS: usize = 512;

pub(super) struct SessionDispatcher {
    // std::sync::Mutex is correct here: the critical section contains only synchronous
    // HashMap operations (retain/get/insert) with no await points.
    pub(super) senders: std::sync::Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
    /// Cumulative count of messages dropped because MAX_SESSION_WORKERS cap was reached.
    /// Visible in structured logs via the warn! on each drop; exposed in the bridge
    /// metrics endpoint (TODO: requires a bridge-side HTTP server).
    sessions_dropped_on_cap: Arc<AtomicU64>,
}

impl SessionDispatcher {
    pub(super) fn new(
        client: HubClient,
        app: Arc<BridgeApp>,
        stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            senders: std::sync::Mutex::new(HashMap::new()),
            client,
            app,
            stop_tx,
            shutdown,
            sessions_dropped_on_cap: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) async fn dispatch(&self, msg: WeixinMessage) {
        let key = session_dispatch_key(&msg);

        // N-04 / F-M1-N04: match the poison-safe style used by
        // `evict_closed_senders` below — recover from a poisoned mutex by
        // taking the inner state instead of panicking the Tokio worker.
        let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());

        // Check if a live worker already exists for this key.
        let needs_new = match senders.get(&key) {
            Some(tx) => tx.is_closed(),
            None => true,
        };

        if needs_new {
            // Evict closed entries only when we need to make room — avoids O(N)
            // retain on every message. The background evict_closed_senders task
            // handles periodic cleanup so the map doesn't grow unbounded.
            if senders.len() >= MAX_SESSION_WORKERS {
                senders.retain(|_, tx| !tx.is_closed());
                if senders.len() >= MAX_SESSION_WORKERS {
                    let total_dropped =
                        self.sessions_dropped_on_cap.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        session_key = %key,
                        cap = MAX_SESSION_WORKERS,
                        active = senders.len(),
                        sessions_dropped_on_cap = total_dropped,
                        "session worker cap reached, dropping message"
                    );
                    return;
                }
            }
            let (tx, rx) = mpsc::channel(DEFAULT_SESSION_QUEUE_SIZE);
            senders.insert(key.clone(), tx.clone());
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            let stop_tx = self.stop_tx.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(run_session_worker(
                key.clone(),
                rx,
                client,
                app,
                stop_tx,
                shutdown,
            ));
        }

        if let Some(tx) = senders.get(&key) {
            match tx.try_send(msg) {
                Ok(_) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(session_key = %key, "session queue full, dropping message");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    /// Remove closed sender entries. Called periodically by a background task
    /// so the map doesn't accumulate dead entries between cap-enforcement evictions.
    pub(super) fn evict_closed_senders(&self) {
        if let Ok(mut senders) = self.senders.lock() {
            senders.retain(|_, tx| !tx.is_closed());
        }
    }

    #[cfg(test)]
    pub(super) fn sender_keys(&self) -> Vec<String> {
        // N-04: tests use the same poison-safe recovery as production code
        // so the helper stays consistent with `dispatch` and
        // `evict_closed_senders`.
        let mut keys: Vec<String> = self
            .senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}
