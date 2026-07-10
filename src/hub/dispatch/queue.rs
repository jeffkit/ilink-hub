//! Backend message queue push helpers.
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::error;

use crate::ilink::types::{HubExt, WeixinMessage};

use super::super::*;

/// Push a prepared message to the per-client queue and update metrics.
/// Public for the MCP `call_agent` tool which needs to push synthetic messages.
pub async fn push_to_queue_pub(
    queue: &Arc<dyn MessageQueue>,
    metrics: &Metrics,
    vtoken: &str,
    msg: WeixinMessage,
) {
    push_to_queue(queue, metrics, vtoken, msg).await;
}

pub(super) async fn push_to_queue(
    queue: &Arc<dyn MessageQueue>,
    metrics: &Metrics,
    vtoken: &str,
    msg: WeixinMessage,
) {
    match queue.push(vtoken, msg).await {
        Ok(false) => {
            metrics.messages_dispatched.fetch_add(1, Ordering::Relaxed);
        }
        Ok(true) => {
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(error = %e, vtoken = %crate::redact_token(vtoken), "failed to push message to queue");
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Broadcast variant: shares the unchanged base via `Arc` and only supplies
/// the per-recipient `context_token` and `ilink_hub_ext`. This is the hot
/// path for the broadcast routing decision; cloning the base through `Arc`
/// keeps the per-recipient cost down to a small owned `String` (the vctx).
pub(super) async fn push_shared_to_queue(
    queue: &Arc<dyn MessageQueue>,
    metrics: &Metrics,
    vtoken: &str,
    base: Arc<WeixinMessage>,
    context_token: Option<String>,
    hub_ext: Option<HubExt>,
) {
    match queue
        .push_shared(vtoken, base, context_token, hub_ext)
        .await
    {
        Ok(false) => {
            metrics.messages_dispatched.fetch_add(1, Ordering::Relaxed);
        }
        Ok(true) => {
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(error = %e, vtoken = %crate::redact_token(vtoken), "failed to push shared message to queue");
            metrics.messages_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
