use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::bridge::connection::hub_response_token_rejected;
use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, HubExt, SendMessageRequest,
    SendMessageResponse,
};

pub(super) enum GetUpdatesOutcome {
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
pub(super) fn sanitize_errmsg(s: Option<&str>) -> Option<String> {
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
pub(super) fn sanitize_field(s: Option<&str>, max_len: usize) -> Option<String> {
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
pub(super) fn classify_sendoutcome(parsed: Option<&SendMessageResponse>) -> SendOutcome {
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
pub(super) fn parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)> {
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

    pub(super) async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
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

    pub(super) async fn sendmessage(&self, req: SendMessageRequest) -> Result<SendOutcome> {
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
pub(super) trait ReplySender: Send + Sync + 'static {
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
pub(super) async fn run_partial_forward_loop<S: ReplySender>(
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
pub(super) async fn send_final_with_retry<S: ReplySender + ?Sized>(
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
