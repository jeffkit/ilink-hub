use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bridge::config::BridgeApp;
use crate::bridge::executor::split_into_parts;
use crate::bridge::protocol::Attachment;
use crate::bridge::transport::{InboundMessage, MediaRef, OutboundReply, Transport};
use crate::bridge::AUTH_ERROR_KEYWORDS;

use super::backoff::{backoff_for, retry_budget};
use super::send::{sanitize_field, send_final_with_retry};
use super::session::HandleError;
use super::BridgeStop;

pub(super) fn dump_inbound_message_for_debug(msg: &InboundMessage) {
    let Ok(flag) = std::env::var("ILINKHUB_BRIDGE_DUMP_MSG") else {
        return;
    };
    let f = flag.trim().to_ascii_lowercase();
    if !matches!(f.as_str(), "1" | "true" | "yes") {
        return;
    }

    let full = serde_json::to_string_pretty(&msg.raw)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialize raw message: {e}\"}}"));
    eprintln!("========== ILINKHUB_BRIDGE_DUMP_MSG: full inbound (JSON) ==========");
    eprintln!("{full}");
    eprintln!("========== end full message ==========");

    eprintln!("---------- media ----------");
    for (i, m) in msg.media.iter().enumerate() {
        eprintln!(
            "  media[{i}]: kind={} url={} filename={:?}",
            m.kind, m.url, m.filename
        );
    }
    eprintln!("========== end media dump ==========");
}

/// Map generic [`MediaRef`]s to the bridge-internal [`Attachment`] shape that
/// `to_agentproc_attachments` consumes. Field-for-field identical.
fn media_to_attachments(media: &[MediaRef]) -> Vec<Attachment> {
    media
        .iter()
        .map(|m| Attachment {
            kind: m.kind.clone(),
            url: m.url.clone(),
            filename: m.filename.clone(),
            mime_type: m.mime_type.clone(),
            size: m.size,
        })
        .collect()
}

#[tracing::instrument(
    skip_all,
    fields(
        from    = msg.from_user.as_deref().unwrap_or("?"),
        ctx     = msg.context_token.as_deref().unwrap_or("(none)"),
        profile = tracing::field::Empty,
    )
)]
pub(super) async fn handle_one_message(
    client: &Arc<dyn Transport>,
    app: &BridgeApp,
    msg: InboundMessage,
    shutdown: CancellationToken,
) -> Result<(), HandleError> {
    dump_inbound_message_for_debug(&msg);

    // Always ignore messages from other bots (message_type 2) to avoid loops.
    if msg.is_from_bot {
        return Ok(());
    }

    // Always require a non-empty text body; inbound media/attachments without
    // text are dropped at this layer.
    let text = match msg.text() {
        Some(t) => t.to_string(),
        None => return Ok(()),
    };
    if text.trim().is_empty() {
        return Ok(());
    }

    let attachments = media_to_attachments(&msg.media);

    let (profile_name, profile, payload) = app
        .resolve(&text)
        .with_context(|| format!("route message for profile: {text:?}"))?;

    let ctx = msg
        .context_token
        .clone()
        .filter(|s| !s.is_empty())
        .context("inbound message missing context_token")?;
    let from_user = msg.from_user.clone().unwrap_or_default();
    // CLI session id to resume. In `via: hub`, the Hub persists the last
    // `cli_session_id` we sent and echoes it back here as `session_id` on the
    // next message of the same session — enabling CLI resume across messages.
    // In `via: direct`, the real iLink upstream does NOT echo this HubExt
    // field, so `session_id` is None and every message starts a fresh CLI
    // session. Restoring resume in direct mode needs a local store keyed by
    // (context_token, session_name) → last cli_session_id (stage 3 seam, not
    // yet implemented).
    let session_for_cli = msg.session_id.clone().unwrap_or_default();
    let session_name_for_cli =
        sanitize_field(msg.session_name.as_deref(), 128).unwrap_or_else(|| "default".to_string());
    // Echoed on outbound sendmessage so Hub can resolve the MCP `call_agent` waiter.
    let a2a_call_id = msg.a2a_call_id.clone().filter(|s| !s.trim().is_empty());
    let is_a2a_inbound = a2a_call_id.is_some();

    tracing::Span::current().record("profile", profile_name);
    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, a2a = is_a2a_inbound, "running bridge profile");

    // watch::channel bounds the partial-chunk buffer to a single slot: only
    // the latest AGENT_PARTIAL chunk matters for UI streaming, and stale
    // intermediate state is dropped automatically. This eliminates the
    // unbounded memory growth that mpsc::unbounded_channel caused when the
    // Hub returned Throttled during a long exponential backoff (up to ~300s).
    let (partial_tx, partial_rx) = watch::channel::<Option<String>>(None);

    let forward_handle = if is_a2a_inbound {
        // A2A: partials must not reach WeChat; the caller's MCP flow surfaces
        // the final reply with caller persona + @mention after the waiter resolves.
        None
    } else {
        let fwd_client = client.clone();
        let fwd_ctx = ctx.clone();
        let fwd_from_user = from_user.clone();
        let fwd_session_name = session_name_for_cli.clone();
        let fwd_shutdown = shutdown.clone();
        let retry_budget = retry_budget(profile.timeout_secs);
        Some(tokio::spawn(super::send::run_partial_forward_loop(
            fwd_client,
            partial_rx,
            fwd_ctx,
            fwd_from_user,
            fwd_session_name,
            fwd_shutdown,
            backoff_for,
            retry_budget,
        )))
    };

    let retry_budget = retry_budget(profile.timeout_secs);

    let ap_attachments = super::agentproc_runner::to_agentproc_attachments(&attachments);
    let cli_result = super::agentproc_runner::run_via_agentproc(
        profile,
        profile_name,
        &payload,
        &session_for_cli,
        &session_name_for_cli,
        &from_user,
        &ap_attachments,
        partial_tx,
    )
    .await;

    if let Some(forward_handle) = forward_handle {
        let _ = forward_handle.await;
    }

    match cli_result {
        Ok((raw_body, cli_session, summary)) => {
            // A1 dedup: in streaming mode with partials already forwarded live
            // to the user, the agent's final `text` body duplicates the
            // streamed content — skip the final send (but still persist the
            // session id). A2A inbound is exempt: its partials are suppressed,
            // so the caller always needs the final body.
            let body_already_delivered =
                !is_a2a_inbound && profile.streaming && summary.partial_count > 0;
            let effective_body = if body_already_delivered {
                String::new()
            } else {
                raw_body
            };
            log_message_handled_success(
                profile_name,
                &session_name_for_cli,
                &summary,
                effective_body.trim().is_empty(),
                is_a2a_inbound,
            );
            if effective_body.trim().is_empty() {
                // Empty body: only send if we need to persist a cli_session_id.
                if let Some(sid) = cli_session {
                    if !sid.trim().is_empty() {
                        let reply = OutboundReply {
                            context_token: ctx,
                            text: String::new(),
                            to_user: from_user,
                            cli_session_id: Some(sid),
                            session_name: Some(session_name_for_cli),
                            a2a_call_id,
                            usage: summary.usage.clone(),
                        };
                        if let Err(e) = send_final_with_retry(
                            &**client,
                            reply,
                            backoff_for,
                            retry_budget,
                            &shutdown,
                            "cli_session_id persistence",
                        )
                        .await
                        {
                            warn!(error = %e, "failed to persist cli_session_id after partial-only reply")
                        }
                    }
                }
                return Ok(());
            }
            if is_a2a_inbound {
                // Single sendmessage with a2a_call_id; Hub suppresses upstream
                // delivery and resolves the caller's MCP waiter instead.
                let reply = OutboundReply {
                    context_token: ctx,
                    text: effective_body,
                    to_user: from_user,
                    cli_session_id: cli_session,
                    session_name: Some(session_name_for_cli),
                    a2a_call_id,
                    usage: summary.usage.clone(),
                };
                send_final_with_retry(
                    &**client,
                    reply,
                    backoff_for,
                    retry_budget,
                    &shutdown,
                    "a2a final reply",
                )
                .await
                .map_err(|e| HandleError::from(e.context("sendmessage a2a reply")))?;
                return Ok(());
            }
            // Split long replies into multiple messages instead of truncating.
            let parts = split_into_parts(&effective_body, profile.max_reply_chars);
            let total = parts.len();
            info!(
                profile = profile_name,
                session_name = %session_name_for_cli,
                reply_parts = total,
                body_bytes = summary.body_bytes,
                partial_count = summary.partial_count,
                duration_ms = summary.duration_ms,
                error_event = summary.error_event,
                "message handled: final reply sent"
            );
            for (i, part) in parts.into_iter().enumerate() {
                let is_last = i + 1 == total;
                // cli_session_id is attached only to the last part so it is persisted once.
                let session_id = if is_last { cli_session.clone() } else { None };
                let reply = OutboundReply {
                    context_token: ctx.clone(),
                    text: part,
                    to_user: from_user.clone(),
                    cli_session_id: session_id,
                    session_name: Some(session_name_for_cli.clone()),
                    a2a_call_id: None,
                    usage: summary.usage.clone(),
                };
                send_final_with_retry(
                    &**client,
                    reply,
                    backoff_for,
                    retry_budget,
                    &shutdown,
                    "final reply",
                )
                .await
                .map_err(|e| HandleError::from(e.context("sendmessage reply")))?;
            }
        }
        Err(e) => {
            error!(error = %e, "CLI failed; sending error reply to user");
            if profile.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let reply = OutboundReply {
                    context_token: ctx,
                    text: err_text,
                    to_user: from_user,
                    cli_session_id: None,
                    session_name: Some(session_name_for_cli.clone()),
                    a2a_call_id: None,
                    usage: None,
                };
                // Use an independent (never-cancelled) token rather than
                // `&shutdown` so a concurrent bridge restart does not
                // silently drop the error reply before it is sent.  The
                // CLI has already exited at this point; the reply is the
                // only user-visible feedback about the failure.  The
                // per-call `retry_budget` still bounds the total
                // wall-clock time spent here, and `main()` will abort
                // the task after its 3 s grace period if needed.
                if let Err(send_e) = send_final_with_retry(
                    &**client,
                    reply,
                    backoff_for,
                    retry_budget,
                    &CancellationToken::new(),
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

fn log_message_handled_success(
    profile_name: &str,
    session_name: &str,
    summary: &super::agentproc_runner::RunSummary,
    body_empty: bool,
    is_a2a: bool,
) {
    if body_empty {
        info!(
            profile = profile_name,
            session_name = session_name,
            partial_count = summary.partial_count,
            cli_session = summary.cli_session_present,
            duration_ms = summary.duration_ms,
            usage = ?summary.usage,
            a2a = is_a2a,
            "message handled: empty final body (streaming partials and/or session-only)"
        );
    } else if is_a2a {
        info!(
            profile = profile_name,
            session_name = session_name,
            body_bytes = summary.body_bytes,
            partial_count = summary.partial_count,
            duration_ms = summary.duration_ms,
            "message handled: a2a final reply"
        );
    }
}
