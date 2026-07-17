//! Generic send-side retry loops and sanitization helpers.
//!
//! Stage 1: this module no longer references any IM wire type. The partial and
//! final reply loops are generic over the [`Transport`] trait and exchange
//! [`OutboundReply`] / [`SendOutcome`] DTOs. The iLink HTTP client and its
//! wire-type conversions live in [`crate::bridge::transport::ilink`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::bridge::transport::{OutboundReply, SendOutcome, Transport};

/// Sanitize an upstream `errmsg` string for safe logging.
///
/// Strips control characters (incl. CR/LF and ANSI escapes) and caps the
/// length so a maliciously long upstream message cannot pollute log lines or
/// buffer memory. Returns `None` when the input is `None` or empty after
/// sanitization.
pub(crate) fn sanitize_errmsg(s: Option<&str>) -> Option<String> {
    const MAX_LEN: usize = 256;
    sanitize_field(s, MAX_LEN)
}

/// Sanitize a free-form string field (`session_name`, future identifiers)
/// for safe logging and bounded memory use.
///
/// Strips control characters (incl. CR/LF and ANSI escapes) and caps the
/// length at `max_len` chars. Returns `None` when the input is `None` or
/// empty after sanitization.
pub(crate) fn sanitize_field(s: Option<&str>, max_len: usize) -> Option<String> {
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

/// Build the partial-reply [`OutboundReply`] from a streamed chunk.
fn partial_reply(ctx: &str, chunk: &str, from_user: &str, session_name: &str) -> OutboundReply {
    OutboundReply {
        context_token: ctx.to_string(),
        text: chunk.to_string(),
        to_user: from_user.to_string(),
        session_name: sanitize_field(Some(session_name), 128),
        ..Default::default()
    }
}

/// Buffered + exponential-backoff retry loop for partial replies.
///
/// Keeps a single `pending: Option<String>` slot. While `pending` is set we are
/// inside a retry cycle: every new chunk from the CLI overwrites `pending` (so
/// we never re-send stale fragments), and we re-issue `send_reply` after an
/// exponential backoff until it lands. `Err` clears `pending` to avoid an
/// infinite loop on permanent transport errors; `Sent` clears `pending` and
/// resets the attempt counter; `Throttled` keeps `pending` and bumps the
/// attempt counter.
///
/// `backoff_fn` is injected as a function pointer so tests can use a much
/// smaller schedule without sleeping for tens of seconds; production passes
/// [`super::backoff::backoff_for`].
///
/// Cancel-safety: every await inside the loop is in a `select!` that observes
/// `shutdown`, so an in-flight sleep or send can be aborted without losing the
/// buffered `pending` to a process panic.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_partial_forward_loop(
    sender: Arc<dyn Transport>,
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
    let mut first_throttle_at: Option<Instant> = None;

    loop {
        if pending.is_none() {
            let chunk = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                result = partial_rx.changed() => match result {
                    Ok(()) => match partial_rx.borrow_and_update().clone() {
                        Some(c) => c,
                        None => continue,
                    },
                    Err(_) => return,
                },
            };
            pending = Some(chunk);
            attempt = 0;
        } else {
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
                            }
                        }
                        Err(_) => return,
                    },
                }
            }
        }

        let Some(chunk) = pending.as_ref() else {
            tracing::warn!("partial forward loop: pending was None at phase 2 entry, skipping");
            continue;
        };
        let reply = partial_reply(&ctx, chunk, &from_user, &session_name);
        let send_result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            r = sender.send_reply(reply) => r,
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

/// Send one fully-built reply, retrying on both `Throttled` and transient
/// transport errors with the same exponential backoff as the partial loop,
/// until it lands, `shutdown` fires, or the cumulative retry budget is
/// exhausted.
///
/// Unlike the partial loop there is no buffering/overwrite: the final reply,
/// `cli_session_id` persistence and CLI-error reply each carry one fixed
/// payload, so we just clone-and-resend the same `OutboundReply` until delivery.
///
/// Returns `Ok(())` in all cases — on delivery, on a clean give-up after the
/// budget is exhausted, or when `shutdown` fires. Callers that map the return
/// value to `HandleError` can treat all outcomes as "best-effort sent; move on".
pub(super) async fn send_final_with_retry(
    sender: &dyn Transport,
    reply: OutboundReply,
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
            r = sender.send_reply(reply.clone()) => r,
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
            Err(e) => {
                let elapsed = start.elapsed();
                if elapsed >= max_total {
                    error!(
                        what,
                        attempt,
                        elapsed_secs = elapsed.as_secs(),
                        budget_secs = max_total.as_secs(),
                        error = %e,
                        "final reply abandoned: retry budget exhausted under persistent transport error"
                    );
                    return Ok(());
                }
                attempt = attempt.saturating_add(1);
                let wait = backoff_fn(attempt);
                warn!(
                    what,
                    attempt,
                    backoff_secs = wait.as_secs(),
                    elapsed_secs = elapsed.as_secs(),
                    error = %e,
                    "final reply transport error; retrying with exponential backoff"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        }
    }
}
