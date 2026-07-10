//! Per-conversation virtual context and HubExt builders.
use tracing::warn;

use crate::ilink::types::HubExt;
use crate::store::Store;

use super::super::*;

/// Resolve (or create) a stable virtual context token for this conversation.
///
/// WeChat/iLink may send a new `real_ctx` on every inbound message even in the same DM.
/// Reusing one vctx per peer/group keeps backend session IDs (Claude `--resume`, etc.)
/// attached across turns.
///
/// All lookups go directly to the DB via `store.find_or_create_vctx`.
pub async fn resolve_vctx_for_message(
    state: &HubState,
    real_ctx: &str,
    peer_user_id: &str,
    group_id: Option<&str>,
    _client_scope: Option<&str>,
) -> String {
    match state
        .store
        .find_or_create_vctx(peer_user_id, group_id, real_ctx)
        .await
    {
        Ok(vctx) => vctx,
        Err(e) => {
            warn!(error = %e, "find_or_create_vctx failed, generating ephemeral vctx");
            format!("vctx_{}", uuid::Uuid::new_v4().simple())
        }
    }
}

/// Build `HubExt` for an outbound message.
///
/// Sessions are scoped to `(vctx, vtoken)` so that each backend has its own independent
/// session namespace for the same WeChat conversation.
///
/// When `session_override` is provided (from a quote-reply), that session is used directly
/// instead of the current active session, so the message is routed to the correct conversation.
pub async fn build_hub_ext_for_vctx(
    store: &Store,
    vctx: &str,
    vtoken: &str,
    session_override: Option<String>,
) -> Option<HubExt> {
    // When a session is pinned via quote-reply, we still need its backend_session_id but
    // can skip the active-session lookup — fetch only the session ID for the pinned name.
    if let Some(name) = session_override.filter(|n| !n.is_empty()) {
        let session_id = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.get_backend_session(vctx, vtoken, &name),
        )
        .await
        {
            Ok(Ok(Some(s))) => {
                let t = s.trim().to_string();
                (!t.is_empty()).then_some(t)
            }
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                warn!("Failed to get backend session from DB for {vctx}/{name}: {e}");
                None
            }
            Err(_) => {
                warn!("Timeout getting backend session from DB for {vctx}/{name}");
                None
            }
        };
        return Some(HubExt {
            session_id,
            session_name: Some(name),
            cli_session_id: None,
            a2a_call_id: None,
            a2a_depth: None,
        });
    }

    // No session override: single JOIN query resolves active session name + backend session ID
    // atomically, eliminating the TOCTOU window between the two separate lookups.
    let (session_name, session_id) = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.get_hub_ext_single(vctx, vtoken),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            warn!("Failed to get hub ext from DB for {vctx}: {e}");
            ("default".to_string(), None)
        }
        Err(_) => {
            warn!("Timeout getting hub ext from DB for {vctx}");
            ("default".to_string(), None)
        }
    };

    Some(HubExt {
        session_id,
        session_name: Some(session_name),
        cli_session_id: None,
        a2a_call_id: None,
        a2a_depth: None,
    })
}

/// Reply text when no AI backend is online.
/// Varies slightly based on whether the user sent a hub command (handled separately)
/// or a regular message.
pub(super) fn build_no_backend_reply(user_text: Option<&str>) -> String {
    let is_command = user_text
        .map(|t| t.trim().starts_with('/'))
        .unwrap_or(false);

    if is_command {
        return messages::UNRECOGNIZED_COMMAND.to_string();
    }

    messages::NO_BACKEND_ONLINE.to_string()
}
