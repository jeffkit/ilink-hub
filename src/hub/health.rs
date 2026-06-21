//! Background health checker — periodically evicts clients that have
//! stopped polling `getupdates` for longer than `timeout`.

use std::sync::atomic::Ordering;
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
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let threshold = now_secs.saturating_sub(OFFLINE_THRESHOLD_SECS);

                    // Mark offline: read last_seen timestamps lock-free, then write
                    // only to the registry for the subset of stale clients.
                    {
                        let stale_vtokens: Vec<String> = state
                            .clients
                            .last_seen
                            .iter()
                            .filter(|e| e.value().load(Ordering::Relaxed) < threshold)
                            .map(|e| e.key().clone())
                            .collect();

                        if !stale_vtokens.is_empty() {
                            let mut registry = state.clients.registry.write().await;
                            for vtoken in &stale_vtokens {
                                registry.mark_offline(vtoken);
                            }
                        }
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
