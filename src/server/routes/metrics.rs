//! Prometheus `/metrics` endpoint.
use axum::{extract::State, http::StatusCode};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::error;

use super::auth::AdminGuard;
use crate::hub::HubState;

// ─── Metrics (Prometheus text format) ────────────────────────────────────────

/// Escape a Prometheus label value per the text exposition format: backslash,
/// double quote, and newline must be escaped, in that order (backslash first,
/// so we do not double-escape the escapes we just wrote).
pub(super) fn escape_label_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Collapse per-vtoken values into per-client-name values. vtokens with no
/// name mapping (deleted clients) all fall into a single `unknown` bucket, so
/// they render as one series instead of one duplicate series per vtoken.
/// Summing keeps a departed client's residual activity visible instead of
/// silently dropping it.
pub(super) fn collapse_by_client<'a, I>(
    entries: I,
    names: &std::collections::HashMap<String, String>,
) -> std::collections::BTreeMap<String, u64>
where
    I: IntoIterator<Item = (&'a String, u64)>,
{
    let mut per_client: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (vtoken, value) in entries {
        let client = match names.get(vtoken) {
            Some(name) => name.clone(),
            None => "unknown".to_string(),
        };
        let slot = per_client.entry(client).or_insert(0);
        *slot = slot.saturating_add(value);
    }
    per_client
}

pub async fn metrics(
    _admin: AdminGuard,
    State(state): State<Arc<HubState>>,
) -> (StatusCode, String) {
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
    // Per-client backpressure attribution. Rendered by hand (not via
    // `render_counter`) because the family carries a `client` label and must
    // not emit an unlabelled `_created` sample.
    out.push_str(
        "# HELP ilink_hub_messages_rejected Messages rejected by per-client backpressure (queue full; oldest retained)\n",
    );
    out.push_str("# TYPE ilink_hub_messages_rejected counter\n");
    // `DashMap` guards only live for one iteration, so the keys are owned
    // before aggregation.
    let rejected_per_vtoken: Vec<(String, u64)> = state
        .metrics
        .messages_rejected_by_client
        .iter()
        .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
        .collect();
    for (client, rejected) in collapse_by_client(
        rejected_per_vtoken.iter().map(|(vtoken, v)| (vtoken, *v)),
        &client_names_by_vtoken,
    ) {
        out.push_str(&format!(
            "ilink_hub_messages_rejected_total{{client=\"{}\"}} {rejected}\n",
            escape_label_value(&client)
        ));
    }

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

    // Client names are operator-supplied, so they must be escaped before being
    // interpolated into a label value — an unescaped quote or newline would
    // produce a line the Prometheus text parser rejects, taking the whole
    // scrape (not just one series) with it.
    let client_label = |vtoken: &str| -> String {
        escape_label_value(
            client_names_by_vtoken
                .get(vtoken)
                .map(String::as_str)
                .unwrap_or("unknown"),
        )
    };

    out.push_str("# HELP ilink_hub_queue_size Current pending message count per client\n");
    out.push_str("# TYPE ilink_hub_queue_size gauge\n");
    for (client, size) in collapse_by_client(
        queue_sizes
            .iter()
            .map(|(vtoken, size)| (vtoken, *size as u64)),
        &client_names_by_vtoken,
    ) {
        out.push_str(&format!(
            "ilink_hub_queue_size{{client=\"{}\"}} {size}\n",
            escape_label_value(&client)
        ));
    }

    // Per-tenant outbound rate limiting. `tokens` is the remaining quota in the
    // client's token bucket (the "how close am I to a 429" signal); `rejected`
    // counts requests the limiter turned away. All three are keyed by client
    // name so cardinality stays bounded by the client registry. A client that
    // has not made an outbound request since startup has no bucket and
    // therefore no series here — absence means "no traffic", not "no limit".
    //
    // Buckets can outlive their client (a vtoken deleted via the admin API
    // keeps its bucket until LRU eviction), and an unnamed bucket would render
    // as `client="unknown"`. Two of those would be a duplicate series, which
    // makes Prometheus reject the *entire* scrape — so only named clients are
    // exported. Dropping the departed client's counters is the right trade:
    // nobody can act on the quota of a client that no longer exists.
    let rate_limits: Vec<_> = state
        .clients
        .rate_limiter
        .snapshot()
        .into_iter()
        .filter(|(vtoken, _)| client_names_by_vtoken.contains_key(vtoken))
        .collect();
    out.push_str("# HELP ilink_hub_ratelimit_tokens Remaining outbound request tokens in this client's bucket\n");
    out.push_str("# TYPE ilink_hub_ratelimit_tokens gauge\n");
    for (vtoken, view) in &rate_limits {
        out.push_str(&format!(
            "ilink_hub_ratelimit_tokens{{client=\"{}\"}} {}\n",
            client_label(vtoken),
            view.tokens
        ));
    }
    out.push_str(
        "# HELP ilink_hub_ratelimit_burst Configured burst capacity of this client's bucket\n",
    );
    out.push_str("# TYPE ilink_hub_ratelimit_burst gauge\n");
    for (vtoken, view) in &rate_limits {
        out.push_str(&format!(
            "ilink_hub_ratelimit_burst{{client=\"{}\"}} {}\n",
            client_label(vtoken),
            view.burst
        ));
    }
    out.push_str("# HELP ilink_hub_ratelimit_rejected_total Outbound bot-API requests rejected by the per-client rate limiter\n");
    out.push_str("# TYPE ilink_hub_ratelimit_rejected counter\n");
    for (vtoken, view) in &rate_limits {
        out.push_str(&format!(
            "ilink_hub_ratelimit_rejected_total{{client=\"{}\"}} {}\n",
            client_label(vtoken),
            view.rejected
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

#[cfg(test)]
mod tests {
    use super::{collapse_by_client, escape_label_value};

    fn name_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(vtoken, name)| (vtoken.to_string(), name.to_string()))
            .collect()
    }

    fn collapse(
        entries: &[(&str, u64)],
        names: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, u64> {
        let owned: Vec<(String, u64)> = entries
            .iter()
            .map(|(vtoken, value)| (vtoken.to_string(), *value))
            .collect();
        collapse_by_client(owned.iter().map(|(k, v)| (k, *v)), &name_map(names))
    }

    #[test]
    fn escapes_backslash_quote_and_newline() {
        assert_eq!(escape_label_value("plain-client"), "plain-client");
        assert_eq!(escape_label_value(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label_value("a\nb"), r"a\nb");
        // Backslash must be escaped first, otherwise the escape we write for a
        // quote would itself be re-escaped into `\\"`.
        assert_eq!(escape_label_value(r"a\b"), r"a\\b");
        assert_eq!(escape_label_value("a\\\"b"), "a\\\\\\\"b");
    }

    /// A client name containing a quote or newline must not be able to break
    /// the exposition format: the emitted line has to keep exactly one label
    /// value and stay on one line.
    #[test]
    fn escaped_names_cannot_inject_extra_lines() {
        let label = escape_label_value("evil\nclient\"x");
        // Every dangerous character comes back escaped.
        assert_eq!(label, r#"evil\nclient\"x"#);
        // The escaped form carries no raw newline, so the exposition line that
        // interpolates it stays a single line.
        assert!(!label.contains('\n'), "label: {label:?}");

        let line = format!("ilink_hub_ratelimit_tokens{{client=\"{label}\"}} 1\n");
        assert_eq!(line.matches('\n').count(), 1, "line: {line:?}");
    }

    /// Two vtokens without a name mapping must share one bucket: two series
    /// with the identical label set are a duplicate timeseries, which makes
    /// Prometheus reject the entire scrape.
    #[test]
    fn collapse_folds_unnamed_vtokens_into_one_unknown_bucket() {
        let collapsed = collapse(&[("vt-a", 1), ("vt-b", 2)], &[]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed.get("unknown"), Some(&3));
    }

    /// Mapped vtokens are bucketed by name and summed per name; the bucket key
    /// is the raw name, so escaping happens once at render time and cannot
    /// split one name into two buckets.
    #[test]
    fn collapse_sums_per_client_name() {
        let collapsed = collapse(
            &[("vt-a", 1), ("vt-b", 2), ("vt-c", 4), ("vt-gone", 8)],
            &[("vt-a", "alpha"), ("vt-b", "alpha"), ("vt-c", "beta")],
        );
        assert_eq!(collapsed.get("alpha"), Some(&3));
        assert_eq!(collapsed.get("beta"), Some(&4));
        assert_eq!(collapsed.get("unknown"), Some(&8));
    }

    /// A real client literally named `unknown` shares the unnamed bucket, so
    /// the family still has exactly one series with that label set.
    #[test]
    fn collapse_merges_a_literal_unknown_name_into_the_unnamed_bucket() {
        let collapsed = collapse(&[("vt-a", 2), ("vt-b", 3)], &[("vt-a", "unknown")]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed.get("unknown"), Some(&5));
    }
}
