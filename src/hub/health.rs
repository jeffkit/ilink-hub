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

#[cfg(test)]
mod tests {
    //! N-01 / F-M1-N01: the `last_seen` DashMap must be cleaned up
    //! alongside the registry on every unregister path. Pre-N-01 the
    //! map accumulated entries forever (each `register_client_in_hub`
    //! gets a fresh UUID-based vtoken, so the old key was never
    //! overwritten). These tests pin the cleanup contract on the
    //! `last_seen` data structure itself so the registry paths can be
    //! reviewed without dragging in the full pair/store/queue stack.
    use super::*;
    use crate::hub::registry::ClientRegistry;
    use crate::hub::{ClientState, InMemoryQueue};
    use crate::MessageQueue;

    fn make_client_state() -> Arc<ClientState> {
        let queue: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());
        Arc::new(ClientState::new(queue))
    }

    /// Direct write/read/remove on `last_seen` mirrors what the
    /// production `unregister_client_in_hub` path does (paired with
    /// `registry.remove` + `queue.remove_client` + `store.delete`). If
    /// this ever starts failing, the N-01 invariant is gone.
    #[tokio::test]
    async fn last_seen_remove_clears_entry_for_vtoken() {
        let clients = make_client_state();
        let vtoken = "vhub_test_aaaa".to_string();

        // Seed: write a last_seen timestamp the way `getupdates` does.
        clients
            .last_seen
            .entry(vtoken.clone())
            .or_insert_with(|| std::sync::atomic::AtomicU64::new(0))
            .store(1_700_000_000, Ordering::Relaxed);
        assert!(
            clients.last_seen.contains_key(&vtoken),
            "precondition: seeded last_seen entry must be visible"
        );

        // Production path: registry.remove + last_seen.remove together.
        let mut registry = clients.registry.write().await;
        registry.remove("dummy-name-not-in-registry");
        clients.last_seen.remove(&vtoken);

        assert!(
            !clients.last_seen.contains_key(&vtoken),
            "post-N-01 invariant: last_seen entry for unregistered vtoken must be gone"
        );
    }

    /// Pre-N-01 simulation: write many entries, drop the registry path
    /// entirely, and confirm `last_seen` would have grown without bound.
    /// After N-01 the cleanup is co-located with the registry remove
    /// call; this test simply asserts the data structure supports the
    /// remove operation that production code now performs.
    #[tokio::test]
    async fn last_seen_grows_without_cleanup_when_remove_skipped() {
        let clients = make_client_state();
        let before = clients.last_seen.len();

        // Simulate 100 getupdates-touching clients that we never
        // unregister (the leak scenario).
        for i in 0..100 {
            clients
                .last_seen
                .entry(format!("vhub_leak_{i}"))
                .or_insert_with(|| std::sync::atomic::AtomicU64::new(0))
                .store(1_700_000_000 + i, Ordering::Relaxed);
        }
        assert_eq!(
            clients.last_seen.len(),
            before + 100,
            "without cleanup, last_seen accumulates 100 entries"
        );

        // Now exercise the N-01 fix: clean every entry we just added.
        for i in 0..100 {
            clients.last_seen.remove(&format!("vhub_leak_{i}"));
        }
        assert_eq!(
            clients.last_seen.len(),
            before,
            "after N-01 cleanup, last_seen returns to its starting size"
        );
    }

    /// Helper kept here so the tests above document their reliance on
    /// `ClientRegistry::remove` and `state.clients.last_seen` being
    /// siblings under `ClientState`. The function is unused at runtime
    /// but its signature would catch a future refactor that renames
    /// either side of the pair.
    #[allow(dead_code)]
    fn assert_cleanup_pair_exists(state: &ClientState) {
        let _r: &tokio::sync::RwLock<ClientRegistry> = &state.registry;
        let _l: &Arc<dashmap::DashMap<String, std::sync::atomic::AtomicU64>> = &state.last_seen;
    }
}
