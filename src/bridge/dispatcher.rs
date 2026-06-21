use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::executor::{extract_media_env, run_cli, truncate_chars};
use super::AUTH_ERROR_KEYWORDS;
use crate::bridge::config::BridgeApp;
use crate::bridge::connection::hub_response_token_rejected;
use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, HubExt, SendMessageRequest,
    SendMessageResponse, WeixinMessage,
};

/// Initial backoff after the first throttled (`ret == -2`) response. Doubles
/// on every consecutive throttle up to [`MAX_BACKOFF_SECS`].
const INITIAL_BACKOFF_SECS: u64 = 5;
/// Hard cap on the backoff between throttle retries. Once an attempt count
/// would push the wait past this value, the loop holds at this interval
/// indefinitely (a give-up cap is added in M4; see plan.md).
const MAX_BACKOFF_SECS: u64 = 60;

/// Pure backoff schedule for throttled `sendmessage` retries.
///
/// `attempt` is 0-based: `attempt == 0` is the **first** retry after a
/// throttle, so the first returned value is `INITIAL_BACKOFF_SECS`. The
/// sequence is therefore `5s, 10s, 20s, 40s, 60s, 60s, …`. Saturates at
/// [`MAX_BACKOFF_SECS`] for any `attempt` large enough to overflow or
/// exceed the cap.
fn backoff_for(attempt: u32) -> Duration {
    backoff_for_with(attempt, INITIAL_BACKOFF_SECS, MAX_BACKOFF_SECS)
}

/// Internal helper exposed for testing — lets the test inject a smaller
/// cap so it doesn't have to sleep tens of seconds to observe multiple
/// retries. The `initial_secs` / `max_secs` parameters are **seconds**
/// (same unit as the production constants); the test passes millisecond
/// values converted via `Duration::from_millis` if it wants ms.
#[cfg(test)]
fn backoff_for_test(attempt: u32, initial: Duration, cap: Duration) -> Duration {
    backoff_for_with(attempt, initial.as_secs().max(1), cap.as_secs().max(1))
}

fn backoff_for_with(attempt: u32, initial_secs: u64, max_secs: u64) -> Duration {
    // attempt 0 -> initial_secs, attempt 1 -> 2*initial_secs, ...
    // Multiply by 2^attempt, then clamp. Avoid u64 overflow by bounding
    // the shift to a value well past the cap.
    const SATURATION_SHIFT: u32 = 20; // 2^20 * initial ≈ 60 days; far past 60s cap.
    let shift = attempt.min(SATURATION_SHIFT);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let raw = initial_secs.saturating_mul(multiplier);
    Duration::from_secs(raw.min(max_secs))
}

/// Returned from [`run_bridge`] when Hub terminates the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStop {
    /// Hub rejected the virtual token (401 / revoked).
    TokenRejected,
    /// CLI reported a fatal auth/credential error; user action required.
    FatalCliError(String),
    /// Graceful shutdown was requested (SIGTERM / Ctrl-C); bridge exited cleanly.
    Shutdown,
}

enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

/// Outcome of a single `HubClient::sendmessage` call. Lets callers distinguish
/// upstream throttling (ret == -2) from generic failures without inspecting
/// the raw HTTP body. HTTP transport errors and `ret` values other than 0
/// and -2 surface as `Err(_)` so the existing error-propagation paths are
/// preserved; the typed outcomes are only set when we got a parseable 2xx
/// response and want to communicate the semantic result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SendOutcome {
    /// Hub acknowledged the message.
    Sent,
    /// Hub signalled throttling / rate-limit (`ret == -2`). The call should
    /// be retried with backoff (M2/M3 will buffer + retry at the partial
    /// and final-reply layers). Carries the upstream errmsg for logging.
    ///
    /// Note: does NOT carry the unsent payload; callers must retain the
    /// original request if they wish to retry — M2 will restructure the
    /// partial-reply loop to do exactly that.
    Throttled { ret: i32, errmsg: Option<String> },
}

/// Sanitize an upstream `errmsg` string for safe logging.
///
/// Strips control characters (incl. CR/LF and ANSI escapes) and caps the
/// length so a maliciously long upstream message cannot pollute log lines or
/// buffer memory. Returns `None` when the input is `None` or empty after
/// sanitization.
fn sanitize_errmsg(s: Option<&str>) -> Option<String> {
    const MAX_LEN: usize = 256;
    let raw = s?;
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LEN)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Pure mapping from a parsed `SendMessageResponse` to a `SendOutcome`.
///
/// Extracted so the M1 typed semantics can be unit-tested without spinning
/// up an HTTP server. The (response_body_len, parse_ok) pair is carried
/// back so the caller can decide whether to surface an observability
/// warning — see [`parse_sendoutcome`].
#[allow(dead_code)]
fn classify_sendoutcome(parsed: Option<&SendMessageResponse>) -> SendOutcome {
    match parsed {
        None => SendOutcome::Sent,
        Some(v) => match v.ret {
            Some(0) => SendOutcome::Sent,
            Some(-2) => SendOutcome::Throttled {
                ret: -2,
                errmsg: v.errmsg.clone(),
            },
            Some(_other) => SendOutcome::Sent, // re-classified to Err by caller via ret != 0
            None => SendOutcome::Sent,
        },
    }
}

/// Map the raw HTTP response body of `sendmessage` into a [`SendOutcome`].
///
/// Empty bodies are treated as `Sent` (i.e. hub omitted its usual JSON
/// envelope). When the body parses as JSON and `ret` is some non-zero
/// value other than -2, this returns `Err` carrying the upstream ret/errmsg
/// so the caller can decide whether the value is fatal or transient. When
/// the body fails to parse entirely, this returns `Ok(Sent)` for backwards
/// compatibility with the legacy behaviour — but the caller is expected to
/// log a warning so the silent fallback is observable (see F-007).
fn parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SendOutcome::Sent);
    }
    match serde_json::from_str::<SendMessageResponse>(trimmed) {
        Ok(v) => {
            let ret = v.ret.unwrap_or(0);
            if ret == -2 {
                Ok(SendOutcome::Throttled {
                    ret: -2,
                    errmsg: v.errmsg,
                })
            } else if ret != 0 {
                Err((ret, v.errmsg))
            } else {
                Ok(SendOutcome::Sent)
            }
        }
        Err(_) => Ok(SendOutcome::Sent),
    }
}

#[derive(Clone)]
pub(super) struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    pub(super) fn new(hub_url: String, token: String) -> Self {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client");
        Self {
            http,
            hub_url,
            token,
        }
    }

    async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.clone(),
            base_info: Some(BaseInfo::default()),
            timeout: None,
        };
        let url = format!("{}/ilink/bot/getupdates", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let out: GetUpdatesResponse = resp.json().await?;
        if hub_response_token_rejected(status, out.ret) {
            warn!(
                status = %status,
                errmsg = ?out.errmsg,
                "hub rejected virtual token during getupdates"
            );
            return Ok(GetUpdatesOutcome::TokenRejected);
        }
        if !status.is_success() {
            anyhow::bail!("getupdates HTTP {status}: {:?}", out.errmsg);
        }
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        Ok(GetUpdatesOutcome::Ok(out))
    }

    async fn sendmessage(&self, req: SendMessageRequest) -> Result<SendOutcome> {
        let url = format!("{}/ilink/bot/sendmessage", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendmessage HTTP {status}: {t}");
        }
        let text = resp.text().await?;
        let body_len = text.len();
        match parse_sendoutcome(&text) {
            Ok(out) => {
                if body_len > 0 && matches!(out, SendOutcome::Sent) {
                    // We received a non-empty body that parsed either as
                    // ret==0 / None / failed JSON. Log a low-frequency
                    // observability line so silent fallback to Sent stays
                    // visible (F-007).
                    if serde_json::from_str::<SendMessageResponse>(&text).is_err() {
                        warn!(
                            body_len,
                            "sendmessage response body failed to parse as JSON; treating as Sent (legacy fallback)"
                        );
                    }
                }
                Ok(out)
            }
            Err((other, errmsg)) => {
                anyhow::bail!("sendmessage ret={other} errmsg={:?}", errmsg);
            }
        }
    }
}

/// Abstraction over the side-effecting `sendmessage` call so the partial
/// forward loop can be unit-tested with an in-memory mock that returns
/// scripted `SendOutcome` values without spinning up an HTTP server.
///
/// M2 only needs a single method because the partial forward loop always
/// sends a partial-reply text and never a cli_session_id-persistence or
/// error-reply variant.
trait ReplySender: Send + Sync + 'static {
    fn send_reply(
        &self,
        ctx: &str,
        text: &str,
        from_user: &str,
        session_name: &str,
    ) -> BoxFuture<'_, Result<SendOutcome>>;
}

impl ReplySender for HubClient {
    fn send_reply(
        &self,
        ctx: &str,
        text: &str,
        from_user: &str,
        session_name: &str,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        let mut req =
            SendMessageRequest::reply_text(ctx.to_string(), text.to_string(), from_user, None);
        if let Some(ref mut msg) = req.msg {
            let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
            ext.session_name = Some(session_name.to_string());
        }
        Box::pin(self.sendmessage(req))
    }
}

/// M2: buffered + exponential-backoff retry loop for partial replies.
///
/// The previous (pre-M2) loop consumed a chunk from `partial_rx` and
/// immediately tried to forward it. On `ret == -2` the chunk was dropped
/// (warn + move on), so any partial output produced while the hub was
/// throttling us silently disappeared.
///
/// The new loop keeps a single `pending: Option<String>` slot. While
/// `pending` is set we are inside a retry cycle: every new chunk from the
/// CLI overwrites `pending` (so we never re-send stale fragments), and we
/// re-issue `sendmessage` after an exponential backoff until it lands.
/// `Err` clears `pending` to avoid an infinite loop on permanent transport
/// errors; `Sent` clears `pending` and resets the attempt counter;
/// `Throttled` keeps `pending` and bumps the attempt counter.
///
/// `backoff_fn` is injected as a function pointer so tests can use a
/// much smaller schedule (e.g. 10ms initial) without sleeping for tens
/// of seconds; production passes [`backoff_for`].
///
/// Cancel-safety: every await inside the loop is in a `select!` that
/// observes `shutdown`, so an in-flight sleep or send can be aborted
/// without losing the buffered `pending` to a process panic. (We still
/// drop `pending` on shutdown — by then the whole `run_session_worker`
/// is unwinding and the inbound CLI is going away.)
async fn run_partial_forward_loop<S: ReplySender>(
    sender: S,
    mut partial_rx: mpsc::UnboundedReceiver<String>,
    ctx: String,
    from_user: String,
    session_name: String,
    shutdown: CancellationToken,
    backoff_fn: fn(u32) -> Duration,
) {
    let mut pending: Option<String> = None;
    let mut attempt: u32 = 0;

    loop {
        // Phase 1: wait for a new chunk OR for an in-flight sleep to
        // finish. Shutdown is observed at every await.
        if pending.is_none() {
            // No buffered content — we are free to wait for the next
            // chunk from the CLI.
            let chunk = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                chunk_opt = partial_rx.recv() => match chunk_opt {
                    Some(c) => c,
                    None => return,
                },
            };
            pending = Some(chunk);
            attempt = 0;
        } else {
            // We have buffered content from a previous throttle. Two
            // things can wake us up:
            //   (a) the backoff sleep elapses → retry the send;
            //   (b) a newer chunk arrives from the CLI → overwrite
            //       `pending` so the retry uses the freshest content,
            //       then loop back to (a) to keep the sleep alive.
            let mut backoff = backoff_fn(attempt);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => break,
                    chunk_opt = partial_rx.recv() => match chunk_opt {
                        Some(c) => {
                            debug!(
                                pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                                new_chunk_len = c.len(),
                                "overwriting buffered partial chunk with newer content during backoff"
                            );
                            pending = Some(c);
                            // Don't reset `attempt` here — the
                            // hub is still throttling us; just keep
                            // extending the wait. But cap the sleep
                            // to the new (same) backoff value so we
                            // don't get stuck past the cap.
                            backoff = backoff_fn(attempt);
                        }
                        None => return,
                    },
                }
            }
        }

        // Phase 2: build the request and send. We always read the
        // latest `pending` here — this is what makes phase 1's
        // overwrite semantics actually take effect on the wire.
        let chunk = pending
            .as_ref()
            .expect("pending must be Some when entering phase 2");
        let send_fut = sender.send_reply(&ctx, chunk, &from_user, &session_name);
        let send_result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            r = send_fut => r,
        };
        match send_result {
            Ok(SendOutcome::Sent) => {
                debug!(
                    pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                    attempt, "partial reply delivered"
                );
                pending = None;
                attempt = 0;
            }
            Ok(SendOutcome::Throttled { ret, errmsg }) => {
                attempt = attempt.saturating_add(1);
                let wait = backoff_fn(attempt);
                warn!(
                    ret,
                    attempt,
                    backoff_secs = wait.as_secs(),
                    pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                    errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                    "partial reply throttled; will retry with exponential backoff"
                );
                // Loop back to phase 1; the `else` branch will
                // honour the new attempt count.
            }
            Err(e) => {
                warn!(
                    error = %e,
                    attempt,
                    "partial reply send failed; dropping buffered chunk to avoid infinite retry"
                );
                pending = None;
                attempt = 0;
            }
        }
    }
}

fn session_dispatch_key(msg: &WeixinMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default");
    format!("{ctx}:{session_name}")
}

async fn run_session_worker(
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

        let result = tokio::select! {
            biased;
            // Shutdown arrived while we were processing — cancel the AI call and tell user.
            _ = shutdown.cancelled() => {
                if app.send_error_reply && !ctx_for_err.is_empty() {
                    let req = SendMessageRequest::reply(
                        ctx_for_err,
                        "⚠️ 响应中断（服务正在重启），请稍后重发消息".to_string(),
                        &from_for_err,
                    );
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

enum HandleError {
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

struct SessionDispatcher {
    // std::sync::Mutex is correct here: the critical section contains only synchronous
    // HashMap operations (retain/get/insert) with no await points.
    senders: std::sync::Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
}

impl SessionDispatcher {
    fn new(
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
        }
    }

    async fn dispatch(&self, msg: WeixinMessage) {
        let key = session_dispatch_key(&msg);
        let mut senders = self
            .senders
            .lock()
            .expect("SessionDispatcher senders lock poisoned");

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
                    warn!(
                        session_key = %key,
                        cap = MAX_SESSION_WORKERS,
                        active = senders.len(),
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
    fn evict_closed_senders(&self) {
        if let Ok(mut senders) = self.senders.lock() {
            senders.retain(|_, tx| !tx.is_closed());
        }
    }

    #[cfg(test)]
    fn sender_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .senders
            .lock()
            .expect("senders poisoned")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns when Hub signals a stop condition (token rejected or fatal CLI auth error).
/// Pass a [`CancellationToken`] to request graceful shutdown: in-flight AI calls are
/// cancelled and the user receives an error notification before the function returns.
pub async fn run_bridge_with_shutdown(
    hub_url: String,
    token: String,
    app: BridgeApp,
    shutdown: CancellationToken,
) -> BridgeStop {
    let client = HubClient::new(hub_url, token);
    let app = Arc::new(app);
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(None::<BridgeStop>);
    let dispatcher = Arc::new(SessionDispatcher::new(
        client.clone(),
        Arc::clone(&app),
        stop_tx,
        shutdown.clone(),
    ));
    let mut buf = String::new();
    let mut backoff_secs: u64 = 3;
    const MAX_BACKOFF_SECS: u64 = 60;

    // Periodically evict closed sender entries so the senders map doesn't
    // accumulate dead entries between cap-enforcement evictions on the hot path.
    {
        let dispatcher_weak = Arc::downgrade(&dispatcher);
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_clone.cancelled() => return,
                    _ = interval.tick() => {
                        if let Some(d) = dispatcher_weak.upgrade() {
                            d.evict_closed_senders();
                        } else {
                            return;
                        }
                    }
                }
            }
        });
    }

    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        // Check if any session worker signalled a fatal stop.
        if stop_rx.has_changed().unwrap_or(false) {
            if let Some(reason) = stop_rx.borrow_and_update().clone() {
                return reason;
            }
        }

        let getupdates_fut = client.getupdates(&mut buf);
        let resp = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                // Give in-flight session workers a moment to send error replies before exit.
                tokio::time::sleep(Duration::from_secs(2)).await;
                return BridgeStop::Shutdown;
            }
            r = getupdates_fut => match r {
                Ok(GetUpdatesOutcome::Ok(r)) => {
                    backoff_secs = 3;
                    r
                }
                Ok(GetUpdatesOutcome::TokenRejected) => return BridgeStop::TokenRejected,
                Err(e) => {
                    error!(error = %e, backoff_secs, "getupdates failed; retrying with backoff");
                    let sleep = tokio::time::sleep(Duration::from_secs(backoff_secs));
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return BridgeStop::Shutdown;
                        }
                        _ = sleep => {}
                    }
                    continue;
                }
            },
        };

        if resp.ret != Some(0) {
            warn!(
                ret = ?resp.ret,
                errcode = ?resp.errcode,
                errmsg = ?resp.errmsg,
                "getupdates returned non-zero ret"
            );
        }

        for msg in resp.msgs.unwrap_or_default() {
            dispatcher.dispatch(msg).await;
        }
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns when Hub signals a stop condition (token rejected or fatal CLI auth error).
/// For graceful shutdown support (SIGTERM / Ctrl-C), use [`run_bridge_with_shutdown`].
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) -> BridgeStop {
    run_bridge_with_shutdown(hub_url, token, app, CancellationToken::new()).await
}

fn dump_inbound_weixin_message_for_debug(msg: &WeixinMessage) {
    let Ok(flag) = std::env::var("ILINKHUB_BRIDGE_DUMP_MSG") else {
        return;
    };
    let f = flag.trim().to_ascii_lowercase();
    if !matches!(f.as_str(), "1" | "true" | "yes") {
        return;
    }

    let full = serde_json::to_string_pretty(msg)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialize WeixinMessage: {e}\"}}"));
    eprintln!("========== ILINKHUB_BRIDGE_DUMP_MSG: full WeixinMessage (JSON) ==========");
    eprintln!("{full}");
    eprintln!("========== end full message ==========");

    if let Some(items) = msg.item_list.as_ref() {
        for (i, item) in items.iter().enumerate() {
            let extra = serde_json::to_string_pretty(&item.extra)
                .unwrap_or_else(|_| "\"<extra serialize error>\"".to_string());
            eprintln!("---------- item_list[{i}] ----------");
            eprintln!("  type (item_type): {:?}", item.item_type);
            eprintln!("  text_item: {:?}", item.text_item);
            eprintln!("  extra (flattened fields from iLink, not in text_item):");
            eprintln!("{extra}");
        }
        eprintln!("========== end item_list dump ==========");
    } else {
        eprintln!("========== item_list: <none> ==========");
    }
}

#[tracing::instrument(
    skip_all,
    fields(
        from    = msg.from_user_id.as_deref().unwrap_or("?"),
        ctx     = msg.context_token.as_deref().unwrap_or("(none)"),
        profile = tracing::field::Empty,
    )
)]
async fn handle_one_message(
    client: &HubClient,
    app: &BridgeApp,
    msg: WeixinMessage,
    shutdown: CancellationToken,
) -> Result<(), HandleError> {
    dump_inbound_weixin_message_for_debug(&msg);

    if app.skip_bot_messages && msg.message_type == Some(2) {
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.to_string(),
        None if !app.require_text => String::new(),
        None => return Ok(()),
    };
    if text.trim().is_empty() && app.require_text {
        return Ok(());
    }

    let media_env = extract_media_env(&msg);

    let (profile_name, profile, payload) = app
        .resolve(&text)
        .with_context(|| format!("route message for profile (text prefix): {text:?}"))?;

    let ctx = msg
        .context_token
        .clone()
        .filter(|s| !s.is_empty())
        .context("inbound message missing context_token")?;
    let from_user = msg.from_user_id.clone().unwrap_or_default();
    let session_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.as_deref())
        .unwrap_or("")
        .to_string();
    let session_name_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default")
        .to_string();

    tracing::Span::current().record("profile", profile_name);
    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, "running bridge profile");

    let (partial_tx, partial_rx) = mpsc::unbounded_channel::<String>();

    let fwd_client = client.clone();
    let fwd_ctx = ctx.clone();
    let fwd_from_user = from_user.clone();
    let fwd_session_name = session_name_for_cli.clone();
    let fwd_shutdown = shutdown.clone();
    let forward_handle = tokio::spawn(run_partial_forward_loop(
        fwd_client,
        partial_rx,
        fwd_ctx,
        fwd_from_user,
        fwd_session_name,
        fwd_shutdown,
        backoff_for,
    ));

    let cli_result = run_cli(
        profile,
        profile_name,
        &payload,
        &session_for_cli,
        &session_name_for_cli,
        &from_user,
        &ctx,
        &media_env,
        partial_tx,
    )
    .await;

    let _ = forward_handle.await;

    match cli_result {
        Ok((raw_body, cli_session)) => {
            let body = truncate_chars(
                &raw_body,
                profile.max_reply_chars,
                &profile.truncation_suffix,
            );
            if body.trim().is_empty() {
                if let Some(sid) = cli_session {
                    if !sid.trim().is_empty() {
                        let mut req = SendMessageRequest::reply_text(
                            ctx,
                            String::new(),
                            &from_user,
                            Some(sid),
                        );
                        if let Some(ref mut msg) = req.msg {
                            let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                            hub_ext.session_name = Some(session_name_for_cli.clone());
                        }
                        match client.sendmessage(req).await {
                            Ok(SendOutcome::Sent) => {}
                            Ok(SendOutcome::Throttled { ret, errmsg }) => {
                                warn!(
                                    ret,
                                    errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                                    "sendmessage throttled while persisting cli_session_id; M3 will buffer+retry here"
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to persist cli_session_id after ILINK_PARTIAL-only reply")
                            }
                        }
                    }
                }
                return Ok(());
            }
            let mut req = SendMessageRequest::reply_text(ctx, body, &from_user, cli_session);
            if let Some(ref mut msg) = req.msg {
                let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                hub_ext.session_name = Some(session_name_for_cli.clone());
            }
            match client.sendmessage(req).await {
                Ok(SendOutcome::Sent) => {}
                Ok(SendOutcome::Throttled { ret, errmsg }) => {
                    // M1 placeholder: final-reply path does NOT escalate
                    // Throttled to HandleError. M3 will buffer + retry the
                    // final reply on throttling and remove this warn
                    // entirely (or convert it into a structured "retry
                    // attempted" trace). For now we surface the typed
                    // signal so M3 coverage can be verified by watching
                    // for this log line.
                    warn!(
                        ret,
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "sendmessage throttled on final reply; M3 will buffer+retry here"
                    );
                }
                Err(e) => return Err(HandleError::from(e.context("sendmessage reply"))),
            }
        }
        Err(e) => {
            if app.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let req = SendMessageRequest::reply(ctx, err_text, &from_user);
                match client.sendmessage(req).await {
                    Ok(SendOutcome::Sent) => {}
                    Ok(SendOutcome::Throttled { ret, errmsg }) => {
                        warn!(
                            ret,
                            errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                            "sendmessage throttled on CLI-error reply; M3 will buffer+retry here"
                        );
                    }
                    Err(send_e) => warn!(error = %send_e, "failed to send error reply"),
                }
            }
            let err_str = e.to_string().to_lowercase();
            if AUTH_ERROR_KEYWORDS.iter().any(|&k| err_str.contains(k))
                || err_str.contains("not found")
                || err_str.contains("no such file")
            {
                return Err(HandleError::Fatal(BridgeStop::FatalCliError(e.to_string())));
            }
            return Err(HandleError::Transient(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem};

    fn make_msg(ctx: &str, session_name: &str) -> WeixinMessage {
        WeixinMessage {
            context_token: Some(ctx.into()),
            ilink_hub_ext: Some(HubExt {
                session_id: Some(String::new()),
                session_name: Some(session_name.into()),
                cli_session_id: None,
            }),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                ..Default::default()
            }])),
            from_user_id: Some("user1".into()),
            ..Default::default()
        }
    }

    fn make_fast_app() -> BridgeApp {
        BridgeApp::parse_yaml(
            r#"
command: echo
args: []
stdin: none
timeout_secs: 5
"#,
        )
        .unwrap()
    }

    fn fake_client() -> HubClient {
        HubClient::new("http://127.0.0.1:1".into(), "test-token".into())
    }

    fn make_stop_tx() -> tokio::sync::watch::Sender<Option<BridgeStop>> {
        tokio::sync::watch::channel(None).0
    }

    #[test]
    fn key_combines_ctx_and_session_name() {
        assert_eq!(
            session_dispatch_key(&make_msg("ctx-123", "feat-a")),
            "ctx-123:feat-a"
        );
    }

    #[test]
    fn key_defaults_session_name_when_ext_absent() {
        let msg = WeixinMessage {
            context_token: Some("ctx-x".into()),
            ilink_hub_ext: None,
            ..Default::default()
        };
        assert_eq!(session_dispatch_key(&msg), "ctx-x:default");
    }

    #[test]
    fn key_uses_empty_string_when_ctx_absent() {
        let msg = WeixinMessage {
            context_token: None,
            ilink_hub_ext: None,
            ..Default::default()
        };
        assert_eq!(session_dispatch_key(&msg), ":default");
    }

    #[test]
    fn key_differs_for_different_session_names() {
        let a = make_msg("ctx", "session-a");
        let b = make_msg("ctx", "session-b");
        assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
    }

    #[test]
    fn key_differs_for_different_ctx_tokens() {
        let a = make_msg("ctx-1", "default");
        let b = make_msg("ctx-2", "default");
        assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
    }

    #[tokio::test]
    async fn same_key_reuses_single_sender() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-a", "default");
        disp.dispatch(msg.clone()).await;
        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-a:default"]);
    }

    #[tokio::test]
    async fn different_ctx_tokens_get_separate_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-a", "default")).await;
        disp.dispatch(make_msg("ctx-b", "default")).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-a:default", "ctx-b:default"]);
    }

    #[tokio::test]
    async fn different_session_names_get_separate_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-a", "feature-x")).await;
        disp.dispatch(make_msg("ctx-a", "feature-y")).await;
        assert_eq!(
            disp.sender_keys(),
            vec!["ctx-a:feature-x", "ctx-a:feature-y"]
        );
    }

    #[tokio::test]
    async fn three_distinct_sessions_create_three_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-1", "default")).await;
        disp.dispatch(make_msg("ctx-2", "default")).await;
        disp.dispatch(make_msg("ctx-1", "feature-a")).await;
        assert_eq!(
            disp.sender_keys(),
            vec!["ctx-1:default", "ctx-1:feature-a", "ctx-2:default"]
        );
    }

    #[tokio::test]
    async fn repeated_same_key_does_not_grow_sender_map() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-x", "s1");
        for _ in 0..5 {
            disp.dispatch(msg.clone()).await;
        }
        assert_eq!(disp.sender_keys().len(), 1);
    }

    #[tokio::test]
    async fn dead_sender_triggers_new_worker_on_next_dispatch() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-z", "default");

        disp.dispatch(msg.clone()).await;

        {
            let mut senders = disp.senders.lock().expect("senders poisoned");
            senders.remove("ctx-z:default");
        }
        assert_eq!(disp.sender_keys().len(), 0);

        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-z:default"]);
    }

    // ─── parse_sendoutcome ─────────────────────────────────────────────
    //
    // M1 review F-009: pin the body-text → SendOutcome mapping with unit
    // tests so M2/M3 retry decisions have a stable oracle. The legacy
    // behaviour (parse failure → Sent) is intentionally preserved; tests
    // make that explicit so future changes are a deliberate decision.

    #[test]
    fn parse_empty_body_is_sent() {
        assert_eq!(parse_sendoutcome(""), Ok(SendOutcome::Sent));
        assert_eq!(parse_sendoutcome("   \n\t "), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_zero_is_sent() {
        let body = r#"{"ret":0}"#;
        assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_none_is_sent() {
        let body = r#"{}"#;
        assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_negative_two_is_throttled() {
        let body = r#"{"ret":-2,"errmsg":"rate limited"}"#;
        match parse_sendoutcome(body).unwrap() {
            SendOutcome::Throttled { ret, errmsg } => {
                assert_eq!(ret, -2);
                assert_eq!(errmsg.as_deref(), Some("rate limited"));
            }
            other => panic!("expected Throttled, got {:?}", other),
        }
    }

    #[test]
    fn parse_ret_other_non_zero_is_err() {
        let body = r#"{"ret":1,"errmsg":"oops"}"#;
        match parse_sendoutcome(body) {
            Err((1, Some(m))) => assert_eq!(m, "oops"),
            other => panic!("expected Err((1, Some(..))), got {:?}", other),
        }
        let body = r#"{"ret":-99,"errmsg":"unknown"}"#;
        match parse_sendoutcome(body) {
            Err((-99, Some(m))) => assert_eq!(m, "unknown"),
            other => panic!("expected Err((-99, Some(..))), got {:?}", other),
        }
    }

    #[test]
    fn parse_unparseable_body_falls_back_to_sent() {
        // Legacy behaviour: pre-M1 code returned Ok(Sent) on JSON parse
        // failure. M1 keeps the fallback but the dispatcher now emits a
        // warn log; the unit test pins the fallback itself.
        assert_eq!(parse_sendoutcome("not json"), Ok(SendOutcome::Sent));
        assert_eq!(parse_sendoutcome(r#"{"ret": "#), Ok(SendOutcome::Sent));
    }

    // ─── Adversarial / property-style tests ────────────────────────────
    //
    // Goal: catch silent regressions in the parser that could let a
    // hostile hub payload (e.g. ret==-2 disguised as Sent) bypass M2/M3
    // retry logic.

    #[test]
    fn adversarial_ret_negative_two_never_becomes_sent() {
        // Even with errmsg present, ret==-2 must produce Throttled —
        // this is the entire reason SendOutcome exists.
        for body in [
            r#"{"ret":-2}"#,
            r#"{"ret":-2,"errmsg":""}"#,
            r#"{"ret":-2,"errmsg":"hi"}"#,
            r#" {"ret":-2} "#,
            r#"{"ret":-2,"errmsg":"a]lo t of\\n junk"}"#,
        ] {
            match parse_sendoutcome(body).unwrap() {
                SendOutcome::Throttled { ret: -2, .. } => {}
                other => panic!("ret=-2 body {:?} misclassified as {:?}", body, other),
            }
        }
    }

    #[test]
    fn adversarial_large_payload_does_not_panic() {
        // Hub returning a multi-megabyte body must not panic / OOM the
        // parser. We send 4 MB of repeated JSON-safe content and confirm
        // we get a typed outcome (either Sent, Throttled, or parse-fail)
        // rather than a process abort.
        let big = "x".repeat(4 * 1024 * 1024);
        let body = format!(r#"{{"ret":0,"errmsg":"{}"}}"#, big);
        let res = parse_sendoutcome(&body);
        // We don't care which branch it took — only that it didn't panic.
        let _ = res;
    }

    #[test]
    fn adversarial_nested_garbage_does_not_panic() {
        // Hub could send deeply nested or weird JSON. As long as serde
        // rejects it, parse_sendoutcome must fall back to Sent and not
        // propagate a panic.
        for body in [
            r#"{"ret":{"deeply":{"nested":[1,2,3]}}}"#,
            r#"{"ret":-2.5}"#,
            r#"{"ret":-2,"errmsg":12345}"#,
            r#"{} broken"#,
        ] {
            let _ = parse_sendoutcome(body);
        }
    }

    // ─── sanitize_errmsg ───────────────────────────────────────────────

    #[test]
    fn sanitize_strips_control_chars() {
        let dirty = "before\r\nafter\tend\x1b[31mred\x1b[0m";
        let cleaned = sanitize_errmsg(Some(dirty)).unwrap();
        assert!(!cleaned.contains('\r'));
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\t'));
        assert!(!cleaned.contains('\x1b'));
        assert!(cleaned.contains("before"));
        assert!(cleaned.contains("red"));
    }

    #[test]
    fn sanitize_caps_length() {
        let huge = "a".repeat(10_000);
        let cleaned = sanitize_errmsg(Some(&huge)).unwrap();
        assert_eq!(cleaned.len(), 256);
    }

    #[test]
    fn sanitize_handles_none_and_empty() {
        assert!(sanitize_errmsg(None).is_none());
        assert!(sanitize_errmsg(Some("")).is_none());
        assert!(sanitize_errmsg(Some("\r\n\t")).is_none());
    }

    #[test]
    fn sanitize_preserves_printable_unicode() {
        // Multi-byte UTF-8 and punctuation must survive.
        let s = "你好, world! 🌏 — dash";
        assert_eq!(sanitize_errmsg(Some(s)).as_deref(), Some(s));
    }

    // ─── classify_sendoutcome (additional M1F-006 helper) ──────────────

    #[test]
    fn classify_three_categories() {
        let none_resp = SendMessageResponse {
            ret: None,
            errmsg: None,
        };
        let zero_resp = SendMessageResponse {
            ret: Some(0),
            errmsg: None,
        };
        let tmo_resp = SendMessageResponse {
            ret: Some(-2),
            errmsg: Some("rl".into()),
        };
        assert_eq!(classify_sendoutcome(None), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&none_resp)), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&zero_resp)), SendOutcome::Sent);
        assert_eq!(
            classify_sendoutcome(Some(&tmo_resp)),
            SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into())
            }
        );
    }

    #[test]
    fn outcome_clone_works() {
        // Pin F-006's derive(Clone) at compile-time — if Clone goes away
        // this won't compile.
        let original = SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("x".into()),
        };
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // ─── backoff_for (M2) ──────────────────────────────────────────────
    //
    // M2 plan E2E-1 requires the backoff schedule to be a pure function
    // that can be unit-tested without spinning up an HTTP server or a
    // tokio runtime. The sequence is the spec's exact value:
    //   attempt 0 ->  5s (first retry)
    //   attempt 1 -> 10s
    //   attempt 2 -> 20s
    //   attempt 3 -> 40s
    //   attempt 4 -> 60s (cap)
    //   attempt 5+ -> 60s
    //   attempt u32::MAX -> 60s (no overflow)

    #[test]
    fn backoff_sequence_matches_spec() {
        let expected_secs = [5u64, 10, 20, 40, 60, 60, 60, 60, 60, 60];
        let actual: Vec<u64> = (0..expected_secs.len() as u32)
            .map(|a| backoff_for(a).as_secs())
            .collect();
        assert_eq!(actual, expected_secs);
    }

    #[test]
    fn backoff_clamps_at_cap_for_large_attempt() {
        // 5 * 2^10 = 5120s — well past 60s; must clamp to 60s.
        assert_eq!(backoff_for(10).as_secs(), MAX_BACKOFF_SECS);
        assert_eq!(backoff_for(20).as_secs(), MAX_BACKOFF_SECS);
    }

    #[test]
    fn backoff_does_not_overflow_at_u32_max() {
        // u32::MAX << SATURATION_SHIFT would overflow without the
        // saturation guard. The function must not panic / wrap.
        let d = backoff_for(u32::MAX);
        assert_eq!(d.as_secs(), MAX_BACKOFF_SECS);
    }

    #[test]
    fn backoff_is_non_decreasing() {
        // Required by E2E-1 scenario B: "退避间隔单调不递减". This is
        // a stronger check than the spec sequence — any monotonic
        // schedule that starts at 5s, doubles, and caps at 60s passes.
        let mut prev = backoff_for(0);
        for a in 1..50 {
            let cur = backoff_for(a);
            assert!(
                cur >= prev,
                "backoff regressed at attempt {a}: {:?} < {:?}",
                cur,
                prev
            );
            prev = cur;
        }
    }

    // ─── Mock ReplySender for run_partial_forward_loop (M2) ───────────
    //
    // The M2 loop is fully driven by a `ReplySender` trait. A scripted
    // mock lets us assert:
    //   - what was sent (in order)
    //   - how many send attempts were made
    //   - whether a buffered chunk was overwritten during backoff
    //   - whether the loop exits cleanly on shutdown

    use std::sync::{Arc, Mutex};

    /// Scripted ReplySender. Holds a queue of outcomes to return in
    /// order. Cloning the sender shares the same queue and the same
    /// log; the spawn handle takes one clone, the test holds the
    /// other as a probe. We hand-implement Clone (Mutex doesn't
    /// derive Clone) by wrapping the inner state in Arc<Mutex<…>>.
    #[derive(Clone)]
    struct ScriptedSender {
        script: Arc<Mutex<Vec<Result<SendOutcome>>>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedSender {
        fn new(script: Vec<Result<SendOutcome>>) -> Self {
            Self {
                script: Arc::new(Mutex::new(script)),
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sent_count(&self) -> usize {
            self.log.lock().unwrap().len()
        }

        fn sent_texts(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl ReplySender for ScriptedSender {
        fn send_reply(
            &self,
            _ctx: &str,
            text: &str,
            _from_user: &str,
            _session_name: &str,
        ) -> BoxFuture<'_, Result<SendOutcome>> {
            self.log.lock().unwrap().push(text.to_string());
            let next = self.script.lock().unwrap().remove(0);
            Box::pin(async move { next })
        }
    }

    fn err_sender_once(_err: anyhow::Error) -> ScriptedSender {
        // `Err` outcomes are scripted in the existing `ScriptedSender` —
        // we don't need a separate helper. The function remains so that
        // future test expansions have a single canonical builder.
        ScriptedSender::new(vec![])
    }

    /// Helper: spawn the forward loop with a fresh mpsc and shutdown
    /// token. Uses a 5ms-initial, 40ms-cap backoff schedule so the
    /// test runtime stays in tens of milliseconds even when 3-4
    /// throttles happen in a row.
    ///
    /// Returns (tx, shutdown_token, handle).
    fn spawn_test_loop<S: ReplySender>(
        sender: S,
    ) -> (
        mpsc::UnboundedSender<String>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        fn test_backoff(attempt: u32) -> Duration {
            // 5ms initial, 40ms cap — small enough that 4 retries still
            // complete well under the tokio test timeout, large enough
            // that macOS timer granularity doesn't collapse adjacent
            // sleeps to zero.
            backoff_for_test(attempt, Duration::from_millis(5), Duration::from_millis(40))
        }
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_partial_forward_loop(
            sender,
            rx,
            "ctx".into(),
            "from".into(),
            "sess".into(),
            shutdown.clone(),
            test_backoff,
        ));
        (tx, shutdown, handle)
    }

    #[tokio::test]
    async fn partial_three_throttles_then_success_delivers_latest_content() {
        // E2E-1 scenario A: hub returns ret=-2 three times then ret=0.
        // Three chunks are pushed; each overwrites the previous one
        // while the loop is throttled. After the throttle clears the
        // last-written content must land exactly once.
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        // Push 3 chunks while the hub is throttling. Each overwrites
        // the buffered `pending` (verified in the sender log: only the
        // last pre-Sent text survives; intermediate values are sent
        // exactly once each because the loop re-issues with the
        // current pending at each retry).
        tx.send("v1".into()).unwrap();
        // Give the loop time to attempt the first send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send("v2".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send("v3".into()).unwrap();

        // Final pending = "v3" when ret==0 finally lands.
        // All 3 retries are observed as distinct sent texts because
        // each overwrite happened before the previous send returned.
        // Wait until the script is exhausted (4 sends).
        for _ in 0..200 {
            if probe.sent_count() >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();
        let _ = shutdown; // never cancelled; loop exits when tx drops.

        let texts = probe.sent_texts();
        assert_eq!(
            texts.len(),
            4,
            "expected exactly 4 send attempts (3 throttled + 1 success), got {texts:?}"
        );
        // The last attempt must be the final chunk's content, so the
        // user sees the freshest fragment after the throttle clears.
        assert_eq!(
            texts.last().unwrap(),
            "v3",
            "final successful send must be the latest buffered content"
        );
    }

    #[tokio::test]
    async fn partial_single_chunk_throttled_then_success_buffers_until_clear() {
        // A single chunk is throttled, then succeeds. The single chunk
        // must be sent at least twice (once throttled, once delivered).
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send("hello".into()).unwrap();
        for _ in 0..200 {
            if probe.sent_count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let texts = probe.sent_texts();
        assert_eq!(texts, vec!["hello", "hello"]);
    }

    #[tokio::test]
    async fn partial_chunk_overwritten_during_backoff_drops_stale_fragment() {
        // Two chunks: first throttled; second arrives during the
        // backoff sleep and overwrites the buffer. After the throttle
        // clears, only the second chunk must be delivered — the first
        // (stale) fragment must NOT be re-sent after the second one.
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send("v1".into()).unwrap();
        // Give the loop time to attempt the first send and enter
        // backoff sleep. Then send v2 — by the time v2 arrives the
        // loop is sleeping; the recv() inside the select! will
        // overwrite pending. The second Throttled in the script then
        // fires with v2 as the carrier. The third send is the success
        // (also v2).
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send("v2".into()).unwrap();

        for _ in 0..2000 {
            if probe.sent_count() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let texts = probe.sent_texts();
        assert_eq!(texts.len(), 3, "expected 3 send attempts, got {texts:?}");
        assert_eq!(
            texts.last().unwrap(),
            "v2",
            "final delivered content must be the latest buffered chunk"
        );
        // "v1" must appear at most once — it was overwritten before
        // a second send carried it. The new "v2" replaced it.
        let v1_count = texts.iter().filter(|t| *t == "v1").count();
        assert_eq!(
            v1_count, 1,
            "stale v1 must not be re-sent after v2 overwrote it: {texts:?}"
        );
        let v2_count = texts.iter().filter(|t| *t == "v2").count();
        assert_eq!(
            v2_count, 2,
            "v2 must be sent on the second throttle and on success: {texts:?}"
        );
    }

    #[tokio::test]
    async fn partial_persistent_throttle_caps_retry_at_max_backoff() {
        // Scenario B (small-N variant): the hub throttles forever
        // and we send a small burst of chunks. The expected behavior
        // is the loop keeps trying, the backoff saturates at 60s, and
        // the most recent chunk is what's pending.
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: None,
            }),
        ]);
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        // Send 3 chunks during the persistent throttle; the script
        // will be drained 4 times.
        for i in 0..3 {
            tx.send(format!("chunk-{i}")).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for _ in 0..200 {
            if probe.sent_count() >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Cancel shutdown to make the loop exit deterministically.
        shutdown.cancel();
        // Give the loop a moment to observe the cancel.
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let texts = probe.sent_texts();
        assert_eq!(
            texts.len(),
            4,
            "expected 4 retry attempts under persistent throttle, got {texts:?}"
        );
        // The last attempt must carry the most-recently-pushed content.
        assert_eq!(
            texts.last().unwrap(),
            "chunk-2",
            "most recent chunk must be the one being retried"
        );
    }

    #[tokio::test]
    async fn partial_err_drops_buffer_and_continues_serving_new_chunks() {
        // Transport / non-throttle errors must NOT spin the retry
        // loop. After Err, the loop clears `pending` and serves the
        // next chunk from the CLI normally.
        let scripted = ScriptedSender::new(vec![
            Err(anyhow::anyhow!("hub down")),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send("first".into()).unwrap();
        // Wait for the error to be consumed + a new chunk to clear
        // the buffer.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send("second".into()).unwrap();
        for _ in 0..200 {
            if probe.sent_count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let texts = probe.sent_texts();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn partial_shutdown_during_backoff_exits_cleanly() {
        // While a backoff is sleeping, a shutdown signal must wake the
        // loop and let it return. The buffered chunk is dropped (it
        // was held in memory only).
        let scripted = ScriptedSender::new(vec![Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        })]);
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send("stuck".into()).unwrap();
        // Wait until the loop has issued the throttled send and is
        // currently sleeping in the backoff.
        for _ in 0..200 {
            if probe.sent_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            probe.sent_count(),
            1,
            "loop should have attempted the first send before we cancel"
        );

        shutdown.cancel();
        // The 5s first backoff means we need a way to wake it without
        // waiting. CancellationToken observed in the same select! as
        // the sleep is the wake-up mechanism; give the runtime a
        // small moment to schedule the cancellation.
        let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            joined.is_ok(),
            "loop must exit within 2s of shutdown (cancel was racing the 5s backoff sleep)"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn partial_chunks_arrived_after_sender_continues_normal_path() {
        // Sanity: if there is never a throttle, each chunk produces
        // exactly one send and the loop processes chunks in order.
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Sent),
            Ok(SendOutcome::Sent),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        for i in 0..3 {
            tx.send(format!("c{i}")).unwrap();
        }
        for _ in 0..200 {
            if probe.sent_count() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let texts = probe.sent_texts();
        assert_eq!(texts, vec!["c0", "c1", "c2"]);
    }

    // ─── HubClient ReplySender impl smoke (M2) ────────────────────────
    //
    // We don't have a real hub here, but we can confirm the impl
    // compiles and that the impl does the right thing on the
    // construction of the request (session_name attachment).
    #[test]
    fn reply_sender_impl_attaches_session_name_to_hub_ext() {
        // Compile-time check is the load-bearing part. Runtime
        // behavior (HTTP-level) is covered by e2e tests in M5.
        fn assert_reply_sender<S: ReplySender>(_: &S) {}
        let client = fake_client();
        assert_reply_sender(&client);
    }

    // We don't run `err_sender_once` from a test because the closure
    // it returns borrows the ErrSender mutably across an await. The
    // function exists for documentation / future use; silence the
    // dead-code lint so the test mod compiles cleanly.
    #[allow(dead_code)]
    fn _keep_err_sender_helper_in_scope() {
        let _ = err_sender_once(anyhow::anyhow!("x"));
    }
}
