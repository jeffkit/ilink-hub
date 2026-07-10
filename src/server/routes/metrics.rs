//! Prometheus `/metrics` endpoint.
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::error;

use super::auth::check_admin_auth;
use crate::hub::HubState;

// ─── Metrics (Prometheus text format) ────────────────────────────────────────

pub async fn metrics(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    if !check_admin_auth(&state.admin, &headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }

    let _hub_name = std::env::var("HUB_NAME").unwrap_or_else(|_| "default".to_string());

    let (online, total, client_names_by_vtoken) = {
        let registry = state.clients.registry.read().await;
        let online = registry.online_clients().len() as u64;
        let total = registry.all_clients().len() as u64;
        let names: std::collections::HashMap<String, String> = registry
            .all_clients()
            .iter()
            .map(|c| (c.vtoken.clone(), c.name.clone()))
            .collect();
        (online, total, names)
    };

    let queue_sizes = state.clients.queue.queue_sizes().await.unwrap_or_else(|e| {
        error!(error = %e, "queue_sizes failed");
        std::collections::HashMap::new()
    });

    let messages_dispatched = state.metrics.messages_dispatched.load(Ordering::Relaxed);
    let messages_dropped = state.metrics.messages_dropped.load(Ordering::Relaxed);
    let messages_persist_dropped = state
        .metrics
        .messages_persist_dropped
        .load(Ordering::Relaxed);
    let upstream_user_messages = state.metrics.upstream_user_messages.load(Ordering::Relaxed);
    let sendmessage_total = state.metrics.sendmessage_total.load(Ordering::Relaxed);
    let sendmessage_errors = state.metrics.sendmessage_errors.load(Ordering::Relaxed);
    let upstream_polls_ok = state.ilink.upstream.polls_ok();
    let upstream_polls_err = state.ilink.upstream.polls_err();
    let relogin_attempts = state.ilink.upstream.relogin_attempts();
    let ilink_status = state.ilink.ilink_status.load(Ordering::Relaxed);
    let created = state.metrics.process_start_unix_secs;

    let mut out = String::with_capacity(2048);

    out.push_str("# HELP ilink_hub_clients_online Number of online clients\n");
    out.push_str("# TYPE ilink_hub_clients_online gauge\n");
    out.push_str(&format!("ilink_hub_clients_online {}\n", online));

    out.push_str("# HELP ilink_hub_clients_total Total registered clients\n");
    out.push_str("# TYPE ilink_hub_clients_total gauge\n");
    out.push_str(&format!("ilink_hub_clients_total {}\n", total));

    render_counter(
        &mut out,
        "ilink_hub_messages_dispatched_total",
        "Messages dispatched",
        messages_dispatched,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_messages_dropped_total",
        "Messages dropped",
        messages_dropped,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_messages_persist_dropped_total",
        "Message history persist tasks dropped due to semaphore exhaustion (DB too slow)",
        messages_persist_dropped,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_user_messages_total",
        "User-side messages received from upstream (excl. bot echo copies)",
        upstream_user_messages,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_polls_ok_total",
        "Successful upstream polls",
        upstream_polls_ok,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_upstream_polls_err_total",
        "Failed upstream polls",
        upstream_polls_err,
        created,
    );

    out.push_str("# HELP ilink_hub_queue_size Current pending message count per client\n");
    out.push_str("# TYPE ilink_hub_queue_size gauge\n");
    for (vtoken, size) in &queue_sizes {
        let name = client_names_by_vtoken
            .get(vtoken)
            .map(String::as_str)
            .unwrap_or("unknown");
        out.push_str(&format!(
            "ilink_hub_queue_size{{client=\"{}\"}} {}\n",
            name, size
        ));
    }

    render_counter(
        &mut out,
        "ilink_hub_sendmessage_total",
        "Total sendmessage calls from backend clients",
        sendmessage_total,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_sendmessage_errors_total",
        "sendmessage calls rejected (unknown token, missing context, etc.)",
        sendmessage_errors,
        created,
    );
    render_counter(
        &mut out,
        "ilink_hub_relogin_attempts_total",
        "Number of QR re-login attempts (manual or automatic)",
        relogin_attempts,
        created,
    );

    out.push_str("# HELP ilink_hub_ilink_status iLink upstream connection status (0=unknown 1=connected 2=needs_login 3=logging_in)\n");
    out.push_str("# TYPE ilink_hub_ilink_status gauge\n");
    out.push_str(&format!("ilink_hub_ilink_status {}\n", ilink_status));

    // Histograms. We render them in Prometheus text format (cumulative
    // bucket counts, plus `_count`, `_sum`, and `_created` siblings). The bucket layout
    // is defined in [`crate::hub::HISTOGRAM_BUCKETS_MS`].
    render_histogram(
        &mut out,
        "ilink_hub_getupdates_latency_ms",
        "Latency of getupdates long-polls (handler entry to drain), in milliseconds",
        &state.metrics.getupdates_latency_ms,
        created,
    );
    render_histogram(
        &mut out,
        "ilink_hub_sendmessage_upstream_latency_ms",
        "Latency of upstream sendmessage HTTP round-trip, in milliseconds",
        &state.metrics.sendmessage_upstream_latency_ms,
        created,
    );
    render_histogram(
        &mut out,
        "ilink_hub_dispatch_latency_ms",
        "Latency of inbound dispatch pipeline (synchronous portion), in milliseconds",
        &state.metrics.dispatch_latency_ms,
        created,
    );

    (StatusCode::OK, out)
}

/// Render a single `LatencyHistogram` as a Prometheus text-format block.
/// Emits:
/// - `<name>_bucket{le="N"} <cumulative_count>` for each boundary + `+Inf`
/// - `<name>_count` total observations
/// - `<name>_sum` total observed **milliseconds** (rounded down from the
///   internally-tracked microsecond sum; see N-02 note on
///   `LatencyHistogram::sum_us`)
/// - `<name>_created` process start timestamp (OpenMetrics convention)
pub(super) fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    h: &crate::hub::LatencyHistogram,
    created: f64,
) {
    use crate::hub::HISTOGRAM_BUCKETS_MS;
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    let mut cumulative: u64 = 0;
    for (i, boundary) in HISTOGRAM_BUCKETS_MS.iter().enumerate() {
        let count = h.buckets[i].load(Ordering::Relaxed);
        cumulative = cumulative.saturating_add(count);
        out.push_str(&format!(
            "{name}_bucket{{le=\"{boundary}\"}} {cumulative}\n"
        ));
    }
    let overflow = h.buckets[HISTOGRAM_BUCKETS_MS.len()].load(Ordering::Relaxed);
    cumulative = cumulative.saturating_add(overflow);
    out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {cumulative}\n"));
    let total = h.count.load(Ordering::Relaxed);
    out.push_str(&format!("{name}_count {total}\n"));
    // sum_us / 1000 keeps the on-the-wire unit (milliseconds) stable for
    // existing Prometheus dashboards while preserving sub-millisecond
    // resolution internally. Sub-millisecond observations now contribute a
    // positive amount after enough observations accumulate (e.g. four
    // 250 μs dispatches contribute 1 to the displayed sum).
    let sum_us = h.sum_us.load(Ordering::Relaxed);
    let sum_ms = sum_us / 1000;
    out.push_str(&format!("{name}_sum {sum_ms}\n"));
    out.push_str(&format!("{name}_created {created}\n"));
}

/// Render a single counter metric in Prometheus text format, including the mandatory
/// `_created` timestamp so scrape tools can compute per-second rates correctly after
/// a process restart (OpenMetrics / Prometheus 2.x `_created` convention).
// `name` must already include the `_total` suffix (Prometheus counter naming convention).
// The `# HELP` and `# TYPE` lines use the base name without `_total` per the spec.
pub(super) fn render_counter(out: &mut String, name: &str, help: &str, value: u64, created: f64) {
    let base = name.strip_suffix("_total").unwrap_or(name);
    out.push_str(&format!("# HELP {base} {help}\n"));
    out.push_str(&format!("# TYPE {base} counter\n"));
    out.push_str(&format!("{name} {value}\n"));
    out.push_str(&format!("{base}_created {created}\n"));
}
