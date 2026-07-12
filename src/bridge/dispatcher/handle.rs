use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bridge::config::BridgeApp;
use crate::bridge::executor::{extract_media_env, run_cli, split_into_parts};
use crate::bridge::AUTH_ERROR_KEYWORDS;
use crate::ilink::types::{HubExt, SendMessageRequest, WeixinMessage};

use super::backoff::{backoff_for, retry_budget};
use super::send::{run_partial_forward_loop, sanitize_field, send_final_with_retry, HubClient};
use super::session::HandleError;
use super::BridgeStop;

pub(super) fn dump_inbound_weixin_message_for_debug(msg: &WeixinMessage) {
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
pub(super) async fn handle_one_message(
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
    // Echoed on outbound sendmessage so Hub can resolve the MCP `call_agent` waiter.
    let a2a_call_id = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.a2a_call_id.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
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
        Some(tokio::spawn(run_partial_forward_loop(
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

    if let Some(forward_handle) = forward_handle {
        let _ = forward_handle.await;
    }

    match cli_result {
        Ok((raw_body, cli_session)) => {
            if raw_body.trim().is_empty() {
                // Empty body: only send if we need to persist a cli_session_id.
                if let Some(sid) = cli_session {
                    if !sid.trim().is_empty() {
                        let mut req = SendMessageRequest::reply_text(
                            ctx,
                            String::new(),
                            &from_user,
                            Some(sid),
                        );
                        attach_outbound_hub_ext(
                            &mut req,
                            &session_name_for_cli,
                            a2a_call_id.as_deref(),
                        );
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
                            warn!(error = %e, "failed to persist cli_session_id after AGENT_PARTIAL-only reply")
                        }
                    }
                }
                return Ok(());
            }
            if is_a2a_inbound {
                // Single sendmessage with a2a_call_id; Hub suppresses upstream
                // delivery and resolves the caller's MCP waiter instead.
                let mut req =
                    SendMessageRequest::reply_text(ctx, raw_body, &from_user, cli_session);
                attach_outbound_hub_ext(&mut req, &session_name_for_cli, a2a_call_id.as_deref());
                send_final_with_retry(
                    client,
                    req,
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
            let parts = split_into_parts(&raw_body, profile.max_reply_chars);
            let total = parts.len();
            for (i, part) in parts.into_iter().enumerate() {
                let is_last = i + 1 == total;
                // cli_session_id is attached only to the last part so it is persisted once.
                let session_id = if is_last { cli_session.clone() } else { None };
                let mut req =
                    SendMessageRequest::reply_text(ctx.clone(), part, &from_user, session_id);
                attach_outbound_hub_ext(&mut req, &session_name_for_cli, None);
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
                // Use an independent (never-cancelled) token rather than
                // `&shutdown` so a concurrent bridge restart does not
                // silently drop the error reply before it is sent.  The
                // CLI has already exited at this point; the reply is the
                // only user-visible feedback about the failure.  The
                // per-call `retry_budget` still bounds the total
                // wall-clock time spent here, and `main()` will abort
                // the task after its 3 s grace period if needed.
                if let Err(send_e) = send_final_with_retry(
                    client,
                    req,
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

/// Attach session metadata (and optional A2A call-id) to an outbound sendmessage.
pub(super) fn attach_outbound_hub_ext(
    req: &mut SendMessageRequest,
    session_name: &str,
    a2a_call_id: Option<&str>,
) {
    if let Some(ref mut msg) = req.msg {
        let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
        hub_ext.session_name = Some(session_name.to_string());
        if let Some(id) = a2a_call_id.filter(|s| !s.is_empty()) {
            hub_ext.a2a_call_id = Some(id.to_string());
        }
    }
}
