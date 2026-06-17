use std::collections::HashMap;
use std::sync::atomic::Ordering;

use prometheus::{IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};

use crate::hub::HubState;

/// Gather all Hub metrics from `state` into a Prometheus text-format string.
///
/// Creates a fresh per-request [`Registry`], registers every metric family,
/// sets current values from [`HubState`], and encodes the result. `hub_name`
/// is used as the `hub` label value on select metrics.
pub async fn gather_metrics(state: &HubState, hub_name: &str) -> Result<String, prometheus::Error> {
    let registry = Registry::new();

    // ── Gauges ──────────────────────────────────────────────────────────────

    let clients_online = IntGaugeVec::new(
        Opts::new("ilink_hub_clients_online", "Number of online clients"),
        &["hub"],
    )?;
    registry.register(Box::new(clients_online.clone()))?;

    let ilink_status = IntGauge::new(
        "ilink_hub_ilink_status",
        "iLink upstream connection status (0=unknown 1=connected 2=needs_login 3=logging_in)",
    )?;
    registry.register(Box::new(ilink_status.clone()))?;

    let ctx_map_size = IntGauge::new(
        "ilink_hub_ctx_map_size",
        "Number of virtual context token entries in memory cache",
    )?;
    registry.register(Box::new(ctx_map_size.clone()))?;

    let queue_size = IntGaugeVec::new(
        Opts::new(
            "ilink_hub_queue_size",
            "Current pending message count per client",
        ),
        &["client"],
    )?;
    registry.register(Box::new(queue_size.clone()))?;

    // ── Counters ────────────────────────────────────────────────────────────

    let clients_total = IntCounterVec::new(
        Opts::new("ilink_hub_clients_total", "Total registered clients"),
        &["hub"],
    )?;
    registry.register(Box::new(clients_total.clone()))?;

    let messages_dispatched = IntCounterVec::new(
        Opts::new("ilink_hub_messages_dispatched_total", "Messages dispatched"),
        &["hub", "cmd"],
    )?;
    registry.register(Box::new(messages_dispatched.clone()))?;

    let messages_dropped = IntCounter::new("ilink_hub_messages_dropped_total", "Messages dropped")?;
    registry.register(Box::new(messages_dropped.clone()))?;

    let upstream_user_messages = IntCounter::new(
        "ilink_hub_upstream_user_messages_total",
        "User-side messages received from upstream (excl. bot echo copies)",
    )?;
    registry.register(Box::new(upstream_user_messages.clone()))?;

    let upstream_polls_ok = IntCounter::new(
        "ilink_hub_upstream_polls_ok_total",
        "Successful upstream polls",
    )?;
    registry.register(Box::new(upstream_polls_ok.clone()))?;

    let upstream_polls_err = IntCounter::new(
        "ilink_hub_upstream_polls_err_total",
        "Failed upstream polls",
    )?;
    registry.register(Box::new(upstream_polls_err.clone()))?;

    let sendmessage_total = IntCounter::new(
        "ilink_hub_sendmessage_total",
        "Total sendmessage calls from backend clients",
    )?;
    registry.register(Box::new(sendmessage_total.clone()))?;

    let sendmessage_errors = IntCounter::new(
        "ilink_hub_sendmessage_errors_total",
        "sendmessage calls rejected (unknown token, missing context, etc.)",
    )?;
    registry.register(Box::new(sendmessage_errors.clone()))?;

    let dispatcher_lagged = IntCounter::new(
        "ilink_hub_dispatcher_lagged_total",
        "Number of messages missed because the dispatcher lagged behind the broadcast channel",
    )?;
    registry.register(Box::new(dispatcher_lagged.clone()))?;

    let relogin_attempts = IntCounter::new(
        "ilink_hub_relogin_attempts_total",
        "Number of QR re-login attempts (manual or automatic)",
    )?;
    registry.register(Box::new(relogin_attempts.clone()))?;

    let persist_faf_failures = IntCounterVec::new(
        Opts::new(
            "ilink_hub_persist_fire_and_forget_failures_total",
            "Fire-and-forget persist_context_token(s)_batch failures on the dispatch path; non-zero rate means context-token mappings were dropped on the floor",
        ),
        &["path"],
    )?;
    registry.register(Box::new(persist_faf_failures.clone()))?;

    // ── Read values from HubState ───────────────────────────────────────────

    let (online, total, client_names_by_vtoken) = {
        let reg = state.clients.registry.read().await;
        let online = reg.online_clients().len() as u64;
        let total = reg.all_clients().len() as u64;
        let names: HashMap<String, String> = reg
            .all_clients()
            .iter()
            .map(|c| (c.vtoken.clone(), c.name.clone()))
            .collect();
        (online, total, names)
    };

    let queue_sizes = state.clients.queue.queue_sizes().await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "queue_sizes failed");
        HashMap::new()
    });

    // ── Set gauge values ────────────────────────────────────────────────────

    clients_online
        .with_label_values(&[hub_name])
        .set(online as i64);

    ilink_status.set(i64::from(state.ilink.ilink_status.load(Ordering::Relaxed)));
    ctx_map_size.set(state.routing.ctx_map.len() as i64);

    for (vtoken, size) in &queue_sizes {
        let name = client_names_by_vtoken
            .get(vtoken)
            .map(String::as_str)
            .unwrap_or("unknown");
        queue_size.with_label_values(&[name]).set(*size as i64);
    }

    // ── Set counter values ──────────────────────────────────────────────────

    clients_total.with_label_values(&[hub_name]).inc_by(total);

    messages_dispatched
        .with_label_values(&[hub_name, "all"])
        .inc_by(state.metrics.messages_dispatched.load(Ordering::Relaxed));

    messages_dropped.inc_by(state.metrics.messages_dropped.load(Ordering::Relaxed));
    upstream_user_messages.inc_by(state.metrics.upstream_user_messages.load(Ordering::Relaxed));
    upstream_polls_ok.inc_by(state.ilink.upstream.polls_ok());
    upstream_polls_err.inc_by(state.ilink.upstream.polls_err());
    sendmessage_total.inc_by(state.metrics.sendmessage_total.load(Ordering::Relaxed));
    sendmessage_errors.inc_by(state.metrics.sendmessage_errors.load(Ordering::Relaxed));
    dispatcher_lagged.inc_by(state.metrics.dispatcher_lagged.load(Ordering::Relaxed));
    relogin_attempts.inc_by(state.ilink.upstream.relogin_attempts());

    persist_faf_failures
        .with_label_values(&["forward_to"])
        .inc_by(
            state
                .metrics
                .persist_fire_and_forget_failures_forward
                .load(Ordering::Relaxed),
        );
    persist_faf_failures
        .with_label_values(&["broadcast"])
        .inc_by(
            state
                .metrics
                .persist_fire_and_forget_failures_broadcast
                .load(Ordering::Relaxed),
        );

    // ── Encode ──────────────────────────────────────────────────────────────

    let encoder = TextEncoder::new();
    let metric_families = registry.gather();
    encoder.encode_to_string(&metric_families)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::ilink::UpstreamClient;
    use crate::store::Store;
    use crate::InMemoryQueue;

    async fn make_test_state() -> Arc<HubState> {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        HubState::new(upstream, Arc::new(store), queue, shutdown_rx)
    }

    #[tokio::test]
    async fn test_all_metrics_present() {
        let state = make_test_state().await;
        let output = gather_metrics(&state, "test-hub").await.unwrap();

        let expected_names = [
            "ilink_hub_clients_online",
            "ilink_hub_clients_total",
            "ilink_hub_messages_dispatched_total",
            "ilink_hub_messages_dropped_total",
            "ilink_hub_upstream_user_messages_total",
            "ilink_hub_upstream_polls_ok_total",
            "ilink_hub_upstream_polls_err_total",
            "ilink_hub_sendmessage_total",
            "ilink_hub_sendmessage_errors_total",
            "ilink_hub_dispatcher_lagged_total",
            "ilink_hub_relogin_attempts_total",
            "ilink_hub_ilink_status",
            "ilink_hub_ctx_map_size",
            "ilink_hub_persist_fire_and_forget_failures_total",
        ];
        for name in &expected_names {
            assert!(output.contains(name), "missing metric: {name}");
        }
    }

    #[tokio::test]
    async fn test_gauge_values_reflect_state() {
        let state = make_test_state().await;
        state.ilink.ilink_status.store(2, Ordering::Relaxed);

        let output = gather_metrics(&state, "myhub").await.unwrap();

        assert!(output.contains("ilink_hub_ilink_status 2"));
        assert!(output.contains("ilink_hub_clients_online{hub=\"myhub\"} 0"));
    }

    #[tokio::test]
    async fn test_counter_values_reflect_metrics() {
        let state = make_test_state().await;

        state.metrics.messages_dropped.store(42, Ordering::Relaxed);
        state.metrics.sendmessage_total.store(7, Ordering::Relaxed);

        let output = gather_metrics(&state, "h1").await.unwrap();

        assert!(output.contains("ilink_hub_messages_dropped_total 42"));
        assert!(output.contains("ilink_hub_sendmessage_total 7"));
    }

    #[tokio::test]
    async fn test_persist_faf_failures_labels() {
        let state = make_test_state().await;

        state
            .metrics
            .persist_fire_and_forget_failures_forward
            .store(3, Ordering::Relaxed);
        state
            .metrics
            .persist_fire_and_forget_failures_broadcast
            .store(5, Ordering::Relaxed);

        let output = gather_metrics(&state, "h").await.unwrap();

        assert!(output
            .contains("ilink_hub_persist_fire_and_forget_failures_total{path=\"forward_to\"} 3"));
        assert!(output
            .contains("ilink_hub_persist_fire_and_forget_failures_total{path=\"broadcast\"} 5"));
    }

    #[tokio::test]
    async fn test_output_format_valid() {
        let state = make_test_state().await;
        let output = gather_metrics(&state, "test").await.unwrap();

        for line in output.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let first_char = line.chars().next().unwrap();
            assert!(
                first_char.is_ascii_alphabetic() || first_char == '_',
                "line does not start with [a-zA-Z_]: {line}"
            );
        }
    }

    #[tokio::test]
    async fn test_hub_label_on_clients_total() {
        let state = make_test_state().await;
        let output = gather_metrics(&state, "prod-hub-1").await.unwrap();

        assert!(output.contains("ilink_hub_clients_total{hub=\"prod-hub-1\"} 0"));
    }

    #[tokio::test]
    async fn test_messages_dispatched_has_hub_and_cmd_labels() {
        let state = make_test_state().await;
        state
            .metrics
            .messages_dispatched
            .store(10, Ordering::Relaxed);

        let output = gather_metrics(&state, "hub-a").await.unwrap();

        assert!(
            output.contains("ilink_hub_messages_dispatched_total{cmd=\"all\",hub=\"hub-a\"} 10")
        );
    }
}
