/// Background health checker — periodically evicts clients that have
/// stopped polling `getupdates` for longer than `timeout`.

use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::HubState;

const OFFLINE_THRESHOLD_SECS: u64 = 90;
const CHECK_INTERVAL_SECS: u64 = 30;

pub fn spawn_health_checker(state: Arc<HubState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;
            let timeout = Duration::from_secs(OFFLINE_THRESHOLD_SECS);
            let mut registry = state.registry.write().await;
            registry.evict_stale(timeout);
            let online = registry.online_clients().len();
            let total = registry.all_clients().len();
            info!(online, total, "health check: client status");
        }
    });
}
