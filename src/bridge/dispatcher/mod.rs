use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bridge::config::BridgeApp;

mod agentproc_runner;
mod backoff;
mod handle;
mod send;
mod session;

use send::{GetUpdatesOutcome, HubClient};
use session::SessionDispatcher;

use crate::bridge::ApprovalBroker;

#[cfg(test)]
use backoff::{backoff_for, backoff_for_test, MAX_BACKOFF_SECS};
#[cfg(test)]
use send::{
    classify_sendoutcome, parse_sendoutcome, run_partial_forward_loop, sanitize_errmsg,
    send_final_with_retry, ReplySender, SendOutcome,
};
#[cfg(test)]
use session::session_dispatch_key;

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
        ApprovalBroker::new(),
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

#[cfg(test)]
mod tests;
