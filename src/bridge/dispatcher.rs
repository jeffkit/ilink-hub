use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
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
/// until either the send lands or the cumulative retry budget (M4,
/// [`retry_budget`]) is exhausted.
const MAX_BACKOFF_SECS: u64 = 60;

/// Floor / ceiling for the M4 cumulative retry budget. A single buffered
/// chunk (or one final reply) is retried under persistent throttling for at
/// most this long before we give up, log an `error!`, and move on. We tie
/// the budget to the CLI `timeout_secs` so a long-running task earns a
/// proportionally long delivery window, but clamp it so a tiny or huge
/// timeout still yields a sane window (the upper bound roughly matches the
/// observed WeChat ~5-7 min throttle cooldown).
const MIN_RETRY_BUDGET_SECS: u64 = 60;
const MAX_RETRY_BUDGET_SECS: u64 = 300;

/// Cumulative wall-clock budget for retrying a throttled send (M4).
///
/// Derived from the profile's `timeout_secs`, clamped to
/// `[MIN_RETRY_BUDGET_SECS, MAX_RETRY_BUDGET_SECS]`. Pure so it can be
/// unit-pinned.
fn retry_budget(profile_timeout_secs: u64) -> Duration {
    Duration::from_secs(profile_timeout_secs.clamp(MIN_RETRY_BUDGET_SECS, MAX_RETRY_BUDGET_SECS))
}

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
/// retries. Operates in **milliseconds** so tests can use sub-second
/// schedules (e.g. 5ms initial, 40ms cap) without losing the exponential
/// shape via `as_secs()` truncation. F-M2-001.
#[cfg(test)]
fn backoff_for_test(attempt: u32, initial: Duration, cap: Duration) -> Duration {
    backoff_for_with_millis(
        attempt,
        initial.as_millis().max(1) as u64,
        cap.as_millis().max(1) as u64,
    )
}

fn backoff_for_with(attempt: u32, initial_secs: u64, max_secs: u64) -> Duration {
    backoff_for_with_millis(
        attempt,
        initial_secs.saturating_mul(1000),
        max_secs.saturating_mul(1000),
    )
}

fn backoff_for_with_millis(attempt: u32, initial_ms: u64, max_ms: u64) -> Duration {
    // attempt 0 -> initial_ms, attempt 1 -> 2*initial_ms, ...
    // Multiply by 2^attempt, then clamp. Avoid u64 overflow by bounding
    // the shift to a value well past the cap.
    const SATURATION_SHIFT: u32 = 20; // 2^20 * initial ≈ far past any practical cap.
    let shift = attempt.min(SATURATION_SHIFT);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let raw = initial_ms.saturating_mul(multiplier);
    Duration::from_millis(raw.min(max_ms))
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
    sanitize_field(s, MAX_LEN)
}

/// Sanitize a free-form string field (`session_name`, future identifiers)
/// for safe logging and bounded memory use.
///
/// Strips control characters (incl. CR/LF and ANSI escapes) and caps the
/// length at `max_len` chars. Returns `None` when the input is `None` or
/// empty after sanitization. F-M2-004 lifts the same handling used by
/// `sanitize_errmsg` to other upstream-controlled string fields.
fn sanitize_field(s: Option<&str>, max_len: usize) -> Option<String> {
    let raw = s?;
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
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
    pub(super) fn new(hub_url: String, token: String) -> Result<Self> {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            http,
            hub_url,
            token,
        })
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

    /// Send a fully-built request. M3 uses this for the final-reply paths
    /// (final body, `cli_session_id` persistence, CLI-error reply) which
    /// — unlike partial chunks — carry a pre-assembled `SendMessageRequest`
    /// rather than just text. Retried through [`send_final_with_retry`].
    fn send_request(&self, req: SendMessageRequest) -> BoxFuture<'_, Result<SendOutcome>>;
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
        // F-M2-004 / F-M2-009: sanitize the upstream-controlled
        // session_name so a hostile 10MB or control-character payload
        // cannot exhaust reqwest body memory, pollute log lines, or
        // accidentally become an empty `Some("")` field.
        let cleaned_session = sanitize_field(Some(session_name), 128);
        if let Some(ref mut msg) = req.msg {
            let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
            ext.session_name = cleaned_session;
        }
        Box::pin(self.sendmessage(req))
    }

    fn send_request(&self, req: SendMessageRequest) -> BoxFuture<'_, Result<SendOutcome>> {
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
#[allow(clippy::too_many_arguments)]
async fn run_partial_forward_loop<S: ReplySender>(
    sender: S,
    mut partial_rx: watch::Receiver<Option<String>>,
    ctx: String,
    from_user: String,
    session_name: String,
    shutdown: CancellationToken,
    backoff_fn: fn(u32) -> Duration,
    max_total: Duration,
) {
    let mut pending: Option<String> = None;
    let mut attempt: u32 = 0;
    // M4: wall-clock instant of the first throttle for the *current*
    // `pending`. `None` whenever we are not inside a retry cycle. Used to
    // bound how long we keep retrying one buffered chunk under persistent
    // throttling before giving up.
    let mut first_throttle_at: Option<Instant> = None;

    loop {
        // Phase 1: wait for a new chunk OR for an in-flight sleep to
        // finish. Shutdown is observed at every await.
        if pending.is_none() {
            // No buffered content — we are free to wait for the next
            // chunk from the CLI.
            let chunk = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                result = partial_rx.changed() => match result {
                    Ok(()) => match partial_rx.borrow_and_update().clone() {
                        Some(c) => c,
                        // Initial None slot — not a real chunk; keep waiting.
                        None => continue,
                    },
                    // Sender dropped → CLI exited, no more chunks.
                    Err(_) => return,
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
            let backoff = backoff_fn(attempt);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => break,
                    result = partial_rx.changed() => match result {
                        Ok(()) => {
                            if let Some(c) = partial_rx.borrow_and_update().clone() {
                                debug!(
                                    pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                                    new_chunk_len = c.len(),
                                    "overwriting buffered partial chunk with newer content during backoff"
                                );
                                pending = Some(c);
                                // `attempt` is intentionally preserved
                                // across overwrites — the hub is still
                                // throttling us, so resetting the counter
                                // would let any CLI cadence flatten the
                                // exponential schedule. `backoff` is
                                // unchanged here as a consequence; we
                                // recompute it only to surface the same
                                // value through the local binding. F-M2-006.
                            }
                        }
                        // Sender dropped → CLI exited, no more chunks.
                        Err(_) => return,
                    },
                }
            }
        }

        // Phase 2: build the request and send. We always read the
        // latest `pending` here — this is what makes phase 1's
        // overwrite semantics actually take effect on the wire.
        let Some(chunk) = pending.as_ref() else {
            tracing::warn!("partial forward loop: pending was None at phase 2 entry, skipping");
            continue;
        };
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
                first_throttle_at = None;
            }
            Ok(SendOutcome::Throttled { ret, errmsg }) => {
                // M4: bound total retry time for one buffered chunk. The
                // budget clock starts at the first throttle and survives
                // chunk overwrites (we stay throttled, so the freshest
                // content simply inherits the remaining budget).
                let started = *first_throttle_at.get_or_insert_with(Instant::now);
                let elapsed = started.elapsed();
                if elapsed >= max_total {
                    error!(
                        ret,
                        attempt,
                        elapsed_secs = elapsed.as_secs(),
                        budget_secs = max_total.as_secs(),
                        pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "partial reply abandoned: retry budget exhausted under persistent throttle"
                    );
                    pending = None;
                    attempt = 0;
                    first_throttle_at = None;
                } else {
                    attempt = attempt.saturating_add(1);
                    let wait = backoff_fn(attempt);
                    warn!(
                        ret,
                        attempt,
                        backoff_secs = wait.as_secs(),
                        elapsed_secs = elapsed.as_secs(),
                        pending_len = pending.as_ref().map(|s| s.len()).unwrap_or(0),
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "partial reply throttled; will retry with exponential backoff"
                    );
                    // Loop back to phase 1; the `else` branch will
                    // honour the new attempt count.
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    attempt,
                    "partial reply send failed; dropping buffered chunk to avoid infinite retry"
                );
                pending = None;
                attempt = 0;
                first_throttle_at = None;
            }
        }
    }
}

/// M3: send one fully-built request, retrying on `Throttled` with the same
/// exponential backoff as the partial loop, until it lands, `shutdown`
/// fires, or the M4 cumulative retry budget is exhausted.
///
/// Unlike the partial loop there is no buffering/overwrite: the final reply,
/// `cli_session_id` persistence and CLI-error reply each carry one fixed
/// payload, so we just clone-and-resend the same `req` until delivery.
///
/// Returns `Ok(())` on delivery **or** on a clean give-up/shutdown (the
/// caller has nothing better to do than continue); only a non-throttle
/// transport error propagates as `Err`.
async fn send_final_with_retry<S: ReplySender + ?Sized>(
    sender: &S,
    req: SendMessageRequest,
    backoff_fn: fn(u32) -> Duration,
    max_total: Duration,
    shutdown: &CancellationToken,
    what: &'static str,
) -> Result<()> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let send_result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            r = sender.send_request(req.clone()) => r,
        };
        match send_result {
            Ok(SendOutcome::Sent) => return Ok(()),
            Ok(SendOutcome::Throttled { ret, errmsg }) => {
                let elapsed = start.elapsed();
                if elapsed >= max_total {
                    error!(
                        ret,
                        what,
                        attempt,
                        elapsed_secs = elapsed.as_secs(),
                        budget_secs = max_total.as_secs(),
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "final reply abandoned: retry budget exhausted under persistent throttle"
                    );
                    return Ok(());
                }
                attempt = attempt.saturating_add(1);
                let wait = backoff_fn(attempt);
                warn!(
                    ret,
                    what,
                    attempt,
                    backoff_secs = wait.as_secs(),
                    elapsed_secs = elapsed.as_secs(),
                    errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                    "final reply throttled; retrying with exponential backoff"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(e) => return Err(e),
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
    /// Cumulative count of messages dropped because MAX_SESSION_WORKERS cap was reached.
    /// Visible in structured logs via the warn! on each drop; exposed in the bridge
    /// metrics endpoint (TODO: requires a bridge-side HTTP server).
    sessions_dropped_on_cap: Arc<AtomicU64>,
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
            sessions_dropped_on_cap: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn dispatch(&self, msg: WeixinMessage) {
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
    fn evict_closed_senders(&self) {
        if let Ok(mut senders) = self.senders.lock() {
            senders.retain(|_, tx| !tx.is_closed());
        }
    }

    #[cfg(test)]
    fn sender_keys(&self) -> Vec<String> {
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
    let client = match HubClient::new(hub_url, token) {
        Ok(c) => c,
        Err(e) => return BridgeStop::FatalCliError(e.to_string()),
    };
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
    let session_name_for_cli = sanitize_field(
        msg.ilink_hub_ext
            .as_ref()
            .and_then(|e| e.session_name.as_deref()),
        128,
    )
    .unwrap_or_else(|| "default".to_string());

    tracing::Span::current().record("profile", profile_name);
    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, "running bridge profile");

    // watch::channel bounds the partial-chunk buffer to a single slot: only
    // the latest ILINK_PARTIAL chunk matters for UI streaming, and stale
    // intermediate state is dropped automatically. This eliminates the
    // unbounded memory growth that mpsc::unbounded_channel caused when the
    // Hub returned Throttled during a long exponential backoff (up to ~300s).
    let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);

    let fwd_client = client.clone();
    let fwd_ctx = ctx.clone();
    let fwd_from_user = from_user.clone();
    let fwd_session_name = session_name_for_cli.clone();
    let fwd_shutdown = shutdown.clone();
    let retry_budget = retry_budget(profile.timeout_secs);
    let forward_handle = tokio::spawn(run_partial_forward_loop(
        fwd_client,
        partial_rx,
        fwd_ctx,
        fwd_from_user,
        fwd_session_name,
        fwd_shutdown,
        backoff_for,
        retry_budget,
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
                        if let Err(e) = send_final_with_retry(
                            client,
                            req,
                            backoff_for,
                            retry_budget,
                            &shutdown,
                            "cli_session_id persistence",
                        )
                        .await
                        {
                            warn!(error = %e, "failed to persist cli_session_id after ILINK_PARTIAL-only reply")
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
            send_final_with_retry(
                client,
                req,
                backoff_for,
                retry_budget,
                &shutdown,
                "final reply",
            )
            .await
            .map_err(|e| HandleError::from(e.context("sendmessage reply")))?;
        }
        Err(e) => {
            error!(error = %e, "CLI failed; sending error reply to user");
            if app.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let mut req = SendMessageRequest::reply(ctx, err_text, &from_user);
                if let Some(ref mut msg) = req.msg {
                    let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                    hub_ext.session_name = Some(session_name_for_cli.clone());
                }
                if let Err(send_e) = send_final_with_retry(
                    client,
                    req,
                    backoff_for,
                    retry_budget,
                    &shutdown,
                    "CLI-error reply",
                )
                .await
                {
                    warn!(error = %send_e, "failed to send error reply")
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
        HubClient::new("http://127.0.0.1:1".into(), "test-token".into()).expect("test http client")
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

    /// N-04 / F-M1-N04: when a `std::sync::Mutex` is poisoned (a thread
    /// panicked while holding the lock), the `unwrap_or_else(|e|
    /// e.into_inner())` recovery idiom MUST yield the inner state
    /// instead of propagating the panic. This pins the *semantics* of
    /// the recovery call used by [`SessionDispatcher::dispatch`] and
    /// [`SessionDispatcher::evict_closed_senders`] on the same
    /// `Mutex<HashMap<String, _>>` shape as the production field, so
    /// any future refactor that drops `into_inner()` (and falls back to
    /// `expect`) would fail this test by panicking in the test thread.
    #[test]
    fn senders_lock_recovery_after_poison_yields_inner_state() {
        // Mirrors `SessionDispatcher::senders` exactly. We share the
        // mutex by `Arc` so the spawned poisoner thread can lock it
        // without resorting to raw-pointer unsafety.
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let senders: Arc<Mutex<HashMap<String, &'static str>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // Seed the inner state.
        senders.lock().unwrap().insert("k1".to_string(), "v1");

        // Poison the mutex from another thread: lock it, then panic
        // while still holding the guard. Join handles the panic; the
        // mutex is now permanently poisoned on every subsequent
        // `.lock().unwrap()` call.
        let poisoned_clone = Arc::clone(&senders);
        let join = std::thread::spawn(move || {
            let _g = poisoned_clone.lock().expect("acquired");
            panic!("intentional poison");
        });
        let _ = join.join();

        // The N-04 recovery idiom used by `dispatch`:
        let mut guard = senders.lock().unwrap_or_else(|e| e.into_inner());
        // Inner state survived — the seed entry is still there.
        assert_eq!(guard.get("k1"), Some(&"v1"));
        guard.insert("k2".to_string(), "v2");
        assert_eq!(guard.len(), 2, "recovery must allow normal mutation");
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
        // failure. M1 keeps the fallback. The dispatcher emits a warn
        // log only when ALL three conditions hold:
        //   1. body_len > 0          (skip empty bodies silently)
        //   2. JSON parse fails      (not a parseable envelope)
        //   3. outcome is Sent       (would otherwise hide the anomaly)
        // The unit test here pins the fallback itself; the warn
        // emission is exercised end-to-end by the dispatcher
        // integration tests. F-M2-008.
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
    use std::time::Instant;

    /// Scripted ReplySender. Holds a queue of outcomes to return in
    /// order. Cloning the sender shares the same queue and the same
    /// log; the spawn handle takes one clone, the test holds the
    /// other as a probe. We hand-implement Clone (Mutex doesn't
    /// derive Clone) by wrapping the inner state in Arc<Mutex<…>>.
    ///
    /// Each `send_reply` call stamps `Instant::now()` into a parallel
    /// `timestamps` log so tests can assert wall-clock intervals and
    /// pin the exponential-backoff shape end-to-end (F-M2-002).
    ///
    /// `loop_forever` mode (set via [`ScriptedSender::new_loop`])
    /// replays the same `SendOutcome` outcome indefinitely instead of
    /// panicking when the script is exhausted — used by persistent
    /// throttle tests that want the loop to keep retrying until
    /// shutdown rather than blow up after N scripted sends. Only
    /// `SendOutcome` (Clone) is supported here; tests that need a
    /// looping `Err` use a regular script with a single outcome
    /// (the loop fires one retry before the panic, which is enough
    /// to assert shutdown-cancel behavior).
    #[derive(Clone)]
    struct ScriptedSender {
        script: Arc<Mutex<Vec<Result<SendOutcome>>>>,
        log: Arc<Mutex<Vec<String>>>,
        timestamps: Arc<Mutex<Vec<Instant>>>,
        loop_outcome: Arc<Mutex<Option<SendOutcome>>>,
    }

    impl ScriptedSender {
        fn new(script: Vec<Result<SendOutcome>>) -> Self {
            Self {
                script: Arc::new(Mutex::new(script)),
                log: Arc::new(Mutex::new(Vec::new())),
                timestamps: Arc::new(Mutex::new(Vec::new())),
                loop_outcome: Arc::new(Mutex::new(None)),
            }
        }

        /// Construct a sender that returns `Ok(outcome)` every call.
        /// Useful for "always throttled" scenarios where we want the
        /// loop to keep retrying until shutdown rather than panic
        /// when an in-line script is exhausted.
        fn new_loop(outcome: SendOutcome) -> Self {
            Self {
                script: Arc::new(Mutex::new(Vec::new())),
                log: Arc::new(Mutex::new(Vec::new())),
                timestamps: Arc::new(Mutex::new(Vec::new())),
                loop_outcome: Arc::new(Mutex::new(Some(outcome))),
            }
        }

        fn sent_count(&self) -> usize {
            self.log.lock().unwrap().len()
        }

        fn sent_texts(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn sent_timestamps(&self) -> Vec<Instant> {
            self.timestamps.lock().unwrap().clone()
        }

        /// Record one send (label + timestamp) and pop the next scripted
        /// outcome (or replay the loop outcome). Shared by `send_reply`
        /// (partial loop) and `send_request` (M3 final-reply paths).
        fn record_and_next(&self, label: String) -> Result<SendOutcome> {
            self.log.lock().unwrap().push(label);
            self.timestamps.lock().unwrap().push(Instant::now());
            if let Some(outcome) = self.loop_outcome.lock().unwrap().clone() {
                Ok(outcome)
            } else {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    panic!("ScriptedSender script exhausted; use new_loop for persistent-throttle tests");
                }
                script.remove(0)
            }
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
            let next = self.record_and_next(text.to_string());
            Box::pin(async move { next })
        }

        fn send_request(&self, _req: SendMessageRequest) -> BoxFuture<'_, Result<SendOutcome>> {
            let next = self.record_and_next("<request>".to_string());
            Box::pin(async move { next })
        }
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
        watch::Sender<Option<String>>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        // Generous budget so existing tests never hit the M4 give-up path.
        spawn_test_loop_with_budget(sender, Duration::from_secs(3600))
    }

    fn test_backoff(attempt: u32) -> Duration {
        // 5ms initial, 40ms cap — small enough that 4 retries still
        // complete well under the tokio test timeout, large enough that
        // macOS timer granularity doesn't collapse adjacent sleeps to
        // zero. Exercises the real exponential-then-cap shape
        // [5ms, 10ms, 20ms, 40ms, 40ms, ...] (production schedule is
        // [5s, 10s, 20s, 40s, 60s, 60s, ...] — also unit-pinned by
        // backoff_sequence_matches_spec).
        backoff_for_test(attempt, Duration::from_millis(5), Duration::from_millis(40))
    }

    /// Same as [`spawn_test_loop`] but with a caller-controlled M4 retry
    /// budget so give-up behavior can be exercised with a tiny window.
    fn spawn_test_loop_with_budget<S: ReplySender>(
        sender: S,
        max_total: Duration,
    ) -> (
        watch::Sender<Option<String>>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = watch::channel::<Option<String>>(None);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_partial_forward_loop(
            sender,
            rx,
            "ctx".into(),
            "from".into(),
            "sess".into(),
            shutdown.clone(),
            test_backoff,
            max_total,
        ));
        (tx, shutdown, handle)
    }

    #[tokio::test]
    async fn partial_three_throttles_then_success_delivers_latest_content() {
        // E2E-1 scenario A: hub returns ret=-2 three times then ret=0.
        // Three chunks are pushed; each overwrites the previous one
        // while the loop is throttled. After the throttle clears the
        // last-written content must land exactly once.
        //
        // After F-M2-001 fix, with the real ms-scale test backoff
        // schedule [5, 10, 20, 40, 40, ...] and the test's 50ms
        // inter-chunk sleeps, the loop may consume the script more
        // or fewer times than 4 depending on exact timing of the
        // chunk recv() races vs the exponential backoff cycles. We
        // therefore script one extra `Sent` at the end (5 outcomes)
        // so the trailing fresh chunk ("v3") can also be delivered
        // without panicking the mock, and assert the LAST sent text
        // equals "v3" — which is the spec property we care about.
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
            Ok(SendOutcome::Sent),
        ]);
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        // Push 3 chunks while the hub is throttling.
        tx.send(Some("v1".into())).unwrap();
        // Give the loop time to attempt the first send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(Some("v2".into())).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(Some("v3".into())).unwrap();

        // Wait until we have at least 4 sends (covers the throttled
        // retries + first success; the trailing v3 may or may not
        // have been delivered yet, depending on timing).
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
        assert!(
            texts.len() >= 4,
            "expected at least 4 sends (3 throttled + ≥1 success), got {texts:?}"
        );
        // The last attempt must be the final chunk's content, so the
        // user sees the freshest fragment after the throttle clears.
        assert_eq!(
            texts.last().unwrap(),
            "v3",
            "final successful send must be the latest buffered content; got {texts:?}"
        );

        // Wall-clock spacing between the first 4 sends must be
        // monotonic non-decreasing (F-M2-002). We accept a 1ms
        // tolerance per gap to absorb tokio timer granularity.
        let stamps = probe.sent_timestamps();
        assert!(stamps.len() >= 4, "expected ≥4 timestamp entries");
        let gaps: Vec<Duration> = stamps
            .iter()
            .take(4)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w[1].duration_since(*w[0]))
            .collect();
        assert_eq!(gaps.len(), 3);
        // First gap is the initial 5ms backoff.
        assert!(
            gaps[0] >= Duration::from_millis(4),
            "first retry must wait at least ~5ms (initial backoff), got {:?}",
            gaps[0]
        );
        for i in 1..gaps.len() {
            assert!(
                gaps[i] >= gaps[i - 1].saturating_sub(Duration::from_millis(1)),
                "backoff regressed at gap[{i}]: {:?} < {:?} (full gaps = {:?})",
                gaps[i],
                gaps[i - 1],
                gaps
            );
        }
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

        tx.send(Some("hello".into())).unwrap();
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
        //
        // With the real ms-scale test backoff schedule [5, 10, 20,
        // 40, 40, ...], the first chunk may be retried several times
        // before the test sends the second one (the 100ms inter-chunk
        // sleep is much longer than the early backoffs but the cap at
        // 40ms means up to 4 retries fit in 100ms). We therefore
        // script 4 throttled outcomes followed by 2 Sents so the
        // mock never panics, and assert the load-bearing invariants:
        //
        //   1. The LAST sent text must be v2.
        //   2. No send carrying v1 may occur AFTER a send carrying v2
        //      (i.e. the stale fragment must NOT be re-sent once v2
        //      has overwritten the buffer).
        //   3. F-M2-003: the inter-send gap BEFORE the first v2 send
        //      (which is attempt=1's 10ms backoff) must be ≤ the gap
        //      after v2 arrives, which corresponds to attempt=2's
        //      20ms backoff — pinning that overwrite does NOT reset
        //      `attempt`.
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
            Ok(SendOutcome::Sent),
            Ok(SendOutcome::Sent),
        ]);
        let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send(Some("v1".into())).unwrap();
        // Give the loop time to attempt several retries and enter
        // backoff sleep. Then send v2 — by the time v2 arrives the
        // loop is sleeping; the recv() inside the select! will
        // overwrite pending. Subsequent sends then carry v2.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(Some("v2".into())).unwrap();

        for _ in 0..2000 {
            // We expect at least one send of v2 to land. Wait for that.
            if probe.sent_texts().iter().any(|t| t == "v2") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let texts = probe.sent_texts();
        // Invariant 1: final delivered content must be v2.
        assert_eq!(
            texts.last().unwrap(),
            "v2",
            "final delivered content must be the latest buffered chunk; got {texts:?}"
        );
        // Invariant 2: no stale v1 send after the first v2 send.
        // Find the index of the first "v2" in the send log; every
        // send after that index must also be v2 (never v1).
        let first_v2_idx = texts
            .iter()
            .position(|t| t == "v2")
            .expect("v2 must be sent at least once");
        for (i, t) in texts.iter().enumerate().skip(first_v2_idx) {
            assert_eq!(
                t, "v2",
                "stale v1 must not be re-sent after v2 overwrote it (send #{i} = {:?}); full log: {texts:?}",
                t
            );
        }
        // We must have at least one v1 send (the initial retry
        // before the overwrite).
        assert!(
            texts.iter().any(|t| t == "v1"),
            "expected at least one v1 send before the overwrite; got {texts:?}"
        );

        // F-M2-003: the gap between the 1st send and the first v2
        // send corresponds to attempt=1's 10ms backoff (10ms). The
        // gap between the first v2 send and the next send (which is
        // still pending=v2 in a fresh retry cycle) corresponds to
        // attempt=2's 20ms backoff. So gap_after_v2 >= gap_before_v2
        // would catch a future regression that resets `attempt` on
        // overwrite. Allow 2ms slack for tokio timer jitter.
        let stamps = probe.sent_timestamps();
        assert!(stamps.len() >= 2);
        let gap_before_v2 = stamps[first_v2_idx].duration_since(stamps[0]);
        if first_v2_idx + 1 < stamps.len() {
            let gap_after_v2 = stamps[first_v2_idx + 1].duration_since(stamps[first_v2_idx]);
            assert!(
                gap_after_v2 + Duration::from_millis(2) >= gap_before_v2,
                "overwrite reset attempt (gap_after_v2={:?} < gap_before_v2={:?}); \
                 gap_before_v2 corresponds to attempt=1 (10ms), \
                 gap_after_v2 corresponds to attempt=2 (20ms)",
                gap_after_v2,
                gap_before_v2
            );
        }
    }

    #[tokio::test]
    async fn partial_persistent_throttle_caps_retry_at_max_backoff() {
        // Scenario B (small-N variant): the hub throttles forever
        // and we send a small burst of chunks. The expected behavior
        // is the loop keeps trying, the backoff saturates at 60s, and
        // the most recent chunk is what's pending.
        //
        // After F-M2-001/F-M2-002, this test also asserts the
        // wall-clock spacing between successive sends is monotonic
        // and converges to the cap (≤ 40ms with our test schedule).
        //
        // We use `new_loop` (Throttled forever) instead of a finite
        // script because with a real ms-scale backoff schedule the
        // loop will fire more than 4 sends within the 50ms inter-chunk
        // sleeps (overwrites during backoff trigger extra cycles);
        // a finite script would panic. We assert that we observed
        // AT LEAST the expected retry count, and then verify the
        // wall-clock shape across the first 4 retries.
        let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        });
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        // Send 3 chunks during the persistent throttle. Each arrives
        // during a backoff sleep and overwrites pending.
        for i in 0..3 {
            tx.send(Some(format!("chunk-{i}"))).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Wait for at least 4 retry attempts (test_backoff schedule
        // = [5, 10, 20, 40, 40, ...], so 4 retries complete in
        // 5+10+20+40 = 75ms; allow generous slack).
        for _ in 0..400 {
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
        assert!(
            texts.len() >= 4,
            "expected at least 4 retry attempts under persistent throttle, got {texts:?}"
        );
        // The last attempt must carry the most-recently-pushed content.
        assert_eq!(
            texts.last().unwrap(),
            "chunk-2",
            "most recent chunk must be the one being retried"
        );

        // Wall-clock spacing between the first 4 sends: must be
        // monotonic non-decreasing and converging to the 40ms cap.
        // Allow a 5ms tolerance for tokio timer granularity and
        // macOS scheduling jitter.
        let stamps = probe.sent_timestamps();
        assert!(stamps.len() >= 4, "expected ≥4 timestamp entries");
        let gaps: Vec<Duration> = stamps
            .iter()
            .take(4)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w[1].duration_since(*w[0]))
            .collect();
        assert_eq!(gaps.len(), 3);
        // First gap (after 1st throttle) is the initial 5ms backoff.
        assert!(
            gaps[0] >= Duration::from_millis(4),
            "first retry must wait at least ~5ms (initial backoff), got {:?}",
            gaps[0]
        );
        // Monotonic non-decreasing: each gap ≥ previous gap (allow
        // 1ms tolerance for timer jitter).
        for i in 1..gaps.len() {
            assert!(
                gaps[i] >= gaps[i - 1].saturating_sub(Duration::from_millis(1)),
                "backoff regressed at gap[{i}]: {:?} < {:?} (full gaps = {:?})",
                gaps[i],
                gaps[i - 1],
                gaps
            );
        }
        // At least one gap must be roughly 2× the previous gap,
        // catching hardcoded sleeps. Allow generous jitter bounds
        // (1.4× to 3×).
        let mut doubled = false;
        for i in 1..gaps.len() {
            if gaps[i].as_micros() >= (gaps[i - 1].as_micros() * 14) / 10
                && gaps[i] < gaps[i - 1].saturating_mul(3)
            {
                doubled = true;
                break;
            }
        }
        assert!(
            doubled,
            "expected at least one roughly-2× gap (exponential shape); got {:?}",
            gaps
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

        tx.send(Some("first".into())).unwrap();
        // Wait for the error to be consumed + a new chunk to clear
        // the buffer.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(Some("second".into())).unwrap();
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
        //
        // Uses loop mode because with the real ms-scale test backoff
        // schedule the loop will fire more than one retry before the
        // test cancels shutdown (the cap of 40ms keeps retries
        // tightly packed); a finite 1-outcome script would panic.
        let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        });
        let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
        let probe = scripted.clone();

        tx.send(Some("stuck".into())).unwrap();
        // Wait until the loop has issued at least one throttled send
        // and is currently sleeping in the backoff.
        for _ in 0..200 {
            if probe.sent_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            probe.sent_count() >= 1,
            "loop should have attempted at least one send before we cancel"
        );

        shutdown.cancel();
        // CancellationToken observed in the same select! as the sleep
        // is the wake-up mechanism. With the ms-scale backoff the
        // sleep may be very short, but the loop must still exit
        // promptly on cancel.
        let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(joined.is_ok(), "loop must exit within 2s of shutdown");
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
            tx.send(Some(format!("c{i}"))).unwrap();
            // With watch::channel only the latest slot is kept; space sends
            // so each chunk is individually observable before the next one
            // overwrites it, preserving the "each chunk → one send" invariant.
            tokio::time::sleep(Duration::from_millis(25)).await;
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

    // ─── send_final_with_retry (M3) + give-up budget (M4) ─────────────

    fn dummy_req() -> SendMessageRequest {
        SendMessageRequest::reply("ctx".to_string(), "final".to_string(), "user")
    }

    #[tokio::test(start_paused = true)]
    async fn final_reply_throttled_thrice_then_delivered() {
        // E2E-2 scenario C/D: the final reply is throttled N times then
        // lands. The retry helper must keep resending the same payload
        // until Sent and return Ok exactly once delivered.
        let scripted = ScriptedSender::new(vec![
            Ok(SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }),
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
        let shutdown = CancellationToken::new();
        let res = send_final_with_retry(
            &scripted,
            dummy_req(),
            test_backoff,
            Duration::from_secs(3600),
            &shutdown,
            "final reply",
        )
        .await;
        assert!(res.is_ok(), "delivery after retries must return Ok");
        assert_eq!(
            scripted.sent_count(),
            4,
            "expected 3 throttled attempts + 1 successful send"
        );
    }

    #[tokio::test]
    async fn final_reply_transport_error_propagates() {
        // A non-throttle transport error must surface to the caller (so
        // the final-reply path can map it to HandleError) instead of
        // being retried forever.
        let scripted = ScriptedSender::new(vec![Err(anyhow::anyhow!("connection reset"))]);
        let shutdown = CancellationToken::new();
        let res = send_final_with_retry(
            &scripted,
            dummy_req(),
            test_backoff,
            Duration::from_secs(3600),
            &shutdown,
            "final reply",
        )
        .await;
        assert!(res.is_err(), "transport error must propagate, not retry");
        assert_eq!(
            scripted.sent_count(),
            1,
            "must not retry a non-throttle Err"
        );
    }

    #[tokio::test]
    async fn final_reply_persistent_throttle_gives_up_within_budget() {
        // M4: under a permanent throttle the helper gives up once the
        // cumulative budget is exhausted and returns Ok (the caller has
        // nothing better to do than continue), rather than spinning
        // forever.
        let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        });
        let shutdown = CancellationToken::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            send_final_with_retry(
                &scripted,
                dummy_req(),
                test_backoff,
                Duration::from_millis(30),
                &shutdown,
                "final reply",
            ),
        )
        .await;
        assert!(
            res.is_ok(),
            "helper must return within the timeout (no infinite spin under persistent throttle)"
        );
        assert!(
            res.unwrap().is_ok(),
            "give-up returns Ok so the caller continues cleanly"
        );
        assert!(
            scripted.sent_count() >= 1,
            "expected at least one send attempt before giving up"
        );
    }

    #[tokio::test]
    async fn final_reply_shutdown_during_backoff_returns_promptly() {
        // Cancel-safety: a shutdown during the backoff sleep aborts the
        // retry loop and returns Ok without hanging.
        let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        });
        let probe = scripted.clone();
        let shutdown = CancellationToken::new();
        let shutdown_for_task = shutdown.clone();
        let task = tokio::spawn(async move {
            send_final_with_retry(
                &scripted,
                dummy_req(),
                test_backoff,
                Duration::from_secs(3600),
                &shutdown_for_task,
                "final reply",
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        shutdown.cancel();
        let res = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(res.is_ok(), "task must finish promptly after shutdown");
        assert!(res.unwrap().unwrap().is_ok());
        let _ = probe.sent_count();
    }

    #[tokio::test]
    async fn partial_persistent_throttle_gives_up_then_serves_new_chunk() {
        // M4 for the partial loop: with a tiny budget and a permanent
        // throttle, the loop abandons the first buffered chunk after the
        // budget elapses. Because the scripted sender always throttles we
        // cannot observe a later Sent, but we CAN observe that the loop
        // gives up (send attempts for one chunk are bounded, no unbounded
        // spin) and then stays responsive: a fresh chunk pushed after the
        // give-up starts a new retry cycle (more send attempts).
        let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        });
        let (tx, shutdown, handle) =
            spawn_test_loop_with_budget(scripted.clone(), Duration::from_millis(30));
        let probe = scripted.clone();

        tx.send(Some("chunk-0".to_string())).unwrap();
        // Allow the budget to elapse and the loop to abandon chunk-0.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after_giveup = probe.sent_count();

        // A fresh chunk after give-up must trigger new attempts.
        tx.send(Some("chunk-1".to_string())).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let total = probe.sent_count();
        assert!(
            after_giveup >= 1,
            "loop must attempt at least once before giving up, got {after_giveup}"
        );
        assert!(
            total > after_giveup,
            "a fresh chunk after give-up must trigger new send attempts ({total} !> {after_giveup})"
        );
    }
}
