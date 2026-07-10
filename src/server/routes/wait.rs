//! Long-poll wait helpers that observe Hub shutdown.
use tokio::sync::watch;
use tracing::error;

use crate::hub::MessageQueue;
use crate::redact_token;

pub(super) async fn wait_notify_or_shutdown(
    queue: &dyn MessageQueue,
    shutdown: &mut watch::Receiver<bool>,
    vtoken: &str,
    poll_secs: u64,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }

    tokio::select! {
        biased;
        _ = wait_shutdown_signal(shutdown) => false,
        notified = async {
            queue.wait_notify(vtoken, poll_secs).await.unwrap_or_else(|e| {
                error!(error = %e, vtoken = %redact_token(vtoken), "wait_notify failed");
                false
            })
        } => notified,
    }
}

pub(super) async fn wait_shutdown_signal(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}
