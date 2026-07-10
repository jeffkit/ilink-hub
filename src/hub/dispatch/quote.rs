//! Quote-reply resolution fallbacks (timestamp → DB → footer).
use std::sync::Arc;
use tracing::{debug, warn};

use super::super::*;

/// Derive the normalised peer scope string (`"peer:<id>"` or `"group:<id>"`) from a message.
///
/// Returns `None` when neither `from_user_id` nor `group_id` is present — the caller
/// should return `None` immediately in that case.
pub(super) fn derive_peer_scope(msg: &crate::ilink::types::WeixinMessage) -> Option<String> {
    let group_id = msg.group_id.as_deref().unwrap_or_default();
    let from_user_id = msg.from_user_id.as_deref().unwrap_or_default();
    if !group_id.is_empty() {
        Some(format!("group:{group_id}"))
    } else if !from_user_id.is_empty() {
        Some(format!("peer:{from_user_id}"))
    } else {
        None
    }
}

/// Timestamp-based quote resolver: use `ref_msg.create_time_ms` to find the assistant message
/// sent at approximately that time. This is the most reliable fallback because iLink always
/// provides a timestamp in `ref_msg.message_item` even when it omits the text content.
pub(super) async fn resolve_quote_from_timestamp(
    state: &Arc<HubState>,
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<QuoteOrigin> {
    let ref_ms = quote_route::collect_quoted_timestamp(msg)?;
    let ref_unix_secs = ref_ms / 1000;
    let peer_user_id = derive_peer_scope(msg)?;
    // Allow ±10 s window to handle minor clock skew between iLink and DB.
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state
            .store
            .find_assistant_message_by_timestamp(&peer_user_id, ref_unix_secs, 10),
    )
    .await
    {
        Ok(Ok(Some((vtoken, session_name)))) if !vtoken.is_empty() => {
            let (name, label) = {
                let registry = state.clients.registry.read().await;
                registry
                    .get_by_vtoken(&vtoken)
                    .map(|c| (c.name.clone(), c.label.clone()))
                    .unwrap_or_else(|| (vtoken.clone(), None))
            };
            debug!(
                peer = %peer_user_id,
                vtoken = %crate::redact_token(&vtoken),
                session = ?session_name,
                ref_unix_secs,
                "resolved via timestamp lookup"
            );
            Some(QuoteOrigin::Client {
                vtoken,
                name,
                label,
                session_name,
            })
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            warn!(error = %e, "timestamp quote lookup failed");
            None
        }
        Err(_) => {
            warn!(peer = %peer_user_id, "timeout in timestamp quote lookup");
            None
        }
    }
}

/// DB-backed quote resolver: query the messages table by peer_user_id + content prefix.
/// Returns the vtoken + session_name recorded when the assistant message was originally sent,
/// independent of footer format.
pub(super) async fn resolve_quote_from_db(
    state: &Arc<HubState>,
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<QuoteOrigin> {
    let (quoted_text, _) = quote_route::collect_quoted(msg)?;
    // Use the same scope normalisation as the quote-index lookup path so the DB
    // query uses the same "peer:<id>" / "group:<id>" format stored by
    // `find_or_create_vctx` / `resolve_send_context`.
    let peer_user_id = derive_peer_scope(msg)?;
    // Use the first 48 chars as prefix (same constant as CONTENT_PREFIX_CHARS in quote_route).
    let prefix: String = quoted_text.trim().chars().take(48).collect();
    if prefix.is_empty() {
        return None;
    }
    // Guard against all-whitespace prefix (e.g. quoted_text built entirely of
    // non-breaking spaces): after the outer trim, an all-whitespace 48-char
    // slice would still trigger a full-table LIKE '% % … %' scan.
    if prefix.trim().is_empty() {
        return None;
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state
            .store
            .find_assistant_message_by_content(&peer_user_id, &prefix),
    )
    .await
    {
        Ok(Ok(Some((vtoken, session_name)))) if !vtoken.is_empty() => {
            let (name, label) = {
                let registry = state.clients.registry.read().await;
                registry
                    .get_by_vtoken(&vtoken)
                    .map(|c| (c.name.clone(), c.label.clone()))
                    .unwrap_or_else(|| (vtoken.clone(), None))
            };
            debug!(
                peer = %peer_user_id,
                vtoken = %crate::redact_token(&vtoken),
                session = ?session_name,
                "resolved via DB message history"
            );
            Some(QuoteOrigin::Client {
                vtoken,
                name,
                label,
                session_name,
            })
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            warn!(error = %e, "DB quote lookup failed, falling back to footer");
            None
        }
        Err(_) => {
            warn!(peer = %peer_user_id, "timeout in DB content quote lookup");
            None
        }
    }
}

/// Fallback quote resolver: parse the origin footer from the quoted message text and look up
/// the backend by name in the client registry.
///
/// Handles both footer formats:
/// - Full: `---\nilink-claude · at-20260615-114019`  → name = "ilink-claude"
/// - Persona (session-only): `---\nat-20260615-114019` → the "name" starts with `at-` or `session-`,
///   meaning it is actually the session identifier from `build_session_only_footer`.
///   In this case look up which backend registered the session in `backend_sessions_v2`.
pub(super) async fn resolve_quote_from_footer(
    state: &Arc<HubState>,
    msg: &crate::ilink::types::WeixinMessage,
) -> Option<QuoteOrigin> {
    let (name, session_name) = quote_route::footer_from_user_quote(msg)?;

    // Fast path: the footer contains an explicit backend name.
    {
        let registry = state.clients.registry.read().await;
        if let Some(client) = registry.get_by_name(&name) {
            debug!(
                backend = %name,
                session = ?session_name,
                "resolved via footer fallback"
            );
            return Some(QuoteOrigin::Client {
                vtoken: client.vtoken.clone(),
                name: client.name.clone(),
                label: client.label.clone(),
                session_name,
            });
        }
    }

    // Slow path: the "name" is actually a session identifier (persona-mode footer like
    // `---\nat-YYYYMMDD-*`).  Look up the owner vtoken via backend_sessions_v2.
    let session_key = if name.starts_with("at-") || name.starts_with("session-") {
        Some(name.as_str())
    } else {
        session_name.as_deref()
    };
    let skey = session_key?;
    let scope = derive_peer_scope(msg)?;
    let vctx = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.store.find_vctx_for_scope(&scope),
    )
    .await
    {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => return None,
        Ok(Err(e)) => {
            warn!(error = %e, "DB scope lookup failed in footer resolver");
            return None;
        }
        Err(_) => {
            warn!(scope = %scope, "timeout in footer resolver scope lookup");
            return None;
        }
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.store.find_vtoken_for_session(&vctx, skey),
    )
    .await
    {
        Ok(Ok(Some(vtoken))) => {
            let (client_name, label) = {
                let registry = state.clients.registry.read().await;
                registry
                    .get_by_vtoken(&vtoken)
                    .map(|c| (c.name.clone(), c.label.clone()))
                    .unwrap_or_else(|| (vtoken.clone(), None))
            };
            debug!(
                session = %skey,
                vtoken = %crate::redact_token(&vtoken),
                "resolved via persona-footer session lookup"
            );
            Some(QuoteOrigin::Client {
                vtoken,
                name: client_name,
                label,
                session_name: Some(skey.to_string()),
            })
        }
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            warn!(error = %e, "DB session lookup failed in footer resolver");
            None
        }
        Err(_) => {
            warn!(session = %skey, "timeout in footer resolver session lookup");
            None
        }
    }
}
