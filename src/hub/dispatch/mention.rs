//! `@backend` temporary mention routing.
use std::sync::Arc;
use tracing::{debug, warn};

use crate::ilink::types::WeixinMessage;

use super::super::*;
use super::hub_ext::{build_hub_ext_for_vctx, resolve_vctx_for_message};
use super::queue::push_to_queue;

/// Handle an `@<backend> <message>` shortcut: forward `payload` to `vtoken` on a brand-new,
/// uniquely-named session, without touching the user's active backend (`/use`) or active
/// session. Each `@` creates a fresh session (product decision); to continue the conversation
/// the user quote-replies to the backend's answer, which the quote index routes back to this
/// session (the echoed `session_name` is registered on the outbound reply).
pub(super) async fn handle_at_mention(
    state: Arc<HubState>,
    mut msg: WeixinMessage,
    backend_name: String,
    vtoken: String,
    payload: String,
) {
    let real_ctx = match msg.context_token.clone() {
        Some(ctx) if !ctx.is_empty() => ctx,
        _ => {
            warn!("@mention message has no context_token, skipping dispatch");
            return;
        }
    };
    let peer_user_id = msg.from_user_id.clone().unwrap_or_default();
    let group_id = msg.group_id.clone();

    let vctx =
        resolve_vctx_for_message(&state, &real_ctx, &peer_user_id, group_id.as_deref(), None).await;

    // Always a new session. Millisecond precision keeps names unique even for rapid @-mentions.
    let session_name = format!("at-{}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f"));

    // Pre-create the (empty-UUID) session slot so it shows up in `/session list` immediately and
    // is a real, resumable session once the backend replies with its cli_session_id. We do NOT
    // mark it active — that would change the user's current session, defeating the "temporary"
    // semantics.
    if let Err(e) = state
        .store
        .set_backend_session(&vctx, &vtoken, &session_name, "")
        .await
    {
        warn!(error = %e, vctx = %vctx, session = %session_name, "failed to pre-create @mention session slot");
    }

    debug!(
        backend = %backend_name,
        vtoken = %crate::redact_token(&vtoken),
        session = %session_name,
        "routing @mention to new session"
    );

    let hub_ext =
        build_hub_ext_for_vctx(&state.store, &vctx, &vtoken, Some(session_name.clone())).await;

    // Strip the `@name` prefix so the backend receives only the message body.
    set_first_text_item(&mut msg, payload);
    msg.context_token = Some(vctx);
    msg.ilink_hub_ext = hub_ext;
    push_to_queue(&state.clients.queue, &state.metrics, &vtoken, msg).await;
}

/// Replace the text of the first text-bearing item in `msg` (used to strip the `@name` prefix
/// before forwarding). If no text item exists, the message is left unchanged.
pub(super) fn set_first_text_item(msg: &mut WeixinMessage, text: String) {
    let Some(items) = msg.item_list.as_mut() else {
        return;
    };
    let items_mut = std::sync::Arc::make_mut(items);
    if let Some(item) = items_mut.iter_mut().find(|i| i.text_item.is_some()) {
        if let Some(ti) = item.text_item.as_mut() {
            ti.text = Some(text);
        }
    }
}
