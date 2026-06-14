//! Background health checker — periodically evicts clients that have
//! stopped polling `getupdates` for longer than `timeout`.

use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::HubState;

const OFFLINE_THRESHOLD_SECS: u64 = 90;
const CHECK_INTERVAL_SECS: u64 = 30;

pub fn spawn_health_checker(state: Arc<HubState>) {
    let mut shutdown = state.ilink.shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("health checker shutting down");
                        return;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)) => {
                    let timeout = Duration::from_secs(OFFLINE_THRESHOLD_SECS);
                    // Hold write lock only for the O(N) eviction scan, then drop it
                    // so getupdates/sendmessage read-lock acquisitions are not blocked
                    // for the subsequent len() calls.
                    {
                        let mut registry = state.clients.registry.write().await;
                        registry.evict_stale(timeout);
                    }
                    let registry = state.clients.registry.read().await;
                    let online = registry.online_clients().len();
                    let total = registry.all_clients().len();
                    info!(online, total, "health check: client status");
                }
            }
        }
    });
}
