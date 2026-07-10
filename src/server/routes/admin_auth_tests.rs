//! Admin auth / AdminGuard unit tests.
use super::auth::*;
use super::metrics::{render_counter, render_histogram};
use crate::hub::AdminConfig;
use axum::http::HeaderMap;

#[tokio::test]
async fn test_check_admin_auth_wrong_token() {
    let admin = AdminConfig {
        token: Some("correct-token".to_string()),
        insecure_no_auth: false,
        outbound_origin_label: None,
    };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer wrong-token-here".parse().unwrap());
    assert!(!check_admin_auth(&admin, &headers));
}

#[tokio::test]
async fn test_check_admin_auth_correct_token() {
    let admin = AdminConfig {
        token: Some("correct-token".to_string()),
        insecure_no_auth: false,
        outbound_origin_label: None,
    };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer correct-token".parse().unwrap());
    assert!(check_admin_auth(&admin, &headers));
}

#[tokio::test]
async fn test_check_admin_auth_empty_headers_no_token_no_insecure() {
    let admin = AdminConfig {
        token: None,
        insecure_no_auth: false,
        outbound_origin_label: None,
    };
    let headers = HeaderMap::new();
    assert!(!check_admin_auth(&admin, &headers));
}

#[tokio::test]
async fn test_check_admin_auth_empty_headers_insecure_mode() {
    let admin = AdminConfig {
        token: None,
        insecure_no_auth: true,
        outbound_origin_label: None,
    };
    let headers = HeaderMap::new();
    assert!(check_admin_auth(&admin, &headers));
}

#[test]
fn test_is_valid_vtoken_accepts_well_formed() {
    // Real UUID v4 simple form, 32 lowercase hex chars.
    assert!(is_valid_vtoken("vhub_0123456789abcdef0123456789abcdef"));
}

#[test]
fn test_is_valid_vtoken_rejects_ilink_style() {
    // SEC-003 hardening: iLink-style bot tokens must never reach the
    // vtoken lookup path.
    assert!(!is_valid_vtoken("botid@im.bot:secret"));
    assert!(!is_valid_vtoken(""));
}

#[test]
fn test_is_valid_vtoken_rejects_wrong_length_and_case() {
    assert!(!is_valid_vtoken("vhub_short"));
    assert!(!is_valid_vtoken("vhub_0123456789ABCDEF0123456789ABCDEF")); // uppercase
    assert!(!is_valid_vtoken("vhub_0123456789abcdef0123456789abcde")); // 31 hex
    assert!(!is_valid_vtoken("vhub_0123456789abcdef0123456789abcdef0")); // 33 hex
}

#[test]
fn test_is_valid_vtoken_rejects_non_hex_suffix() {
    assert!(!is_valid_vtoken("vhub_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
}

#[test]
fn test_extract_vtoken_filters_invalid_format() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer botid@im.bot:secret".parse().unwrap(),
    );
    assert!(extract_vtoken(&headers).is_none());
}

// ── render_counter ────────────────────────────────────────────────────────

/// `_total` suffix must be stripped from the base name so Prometheus can
/// synthesise `_created` from the same base rather than creating
/// `messages_dispatched_total_created`.
#[test]
fn render_counter_strips_total_suffix_from_base_name() {
    let mut out = String::new();
    render_counter(&mut out, "messages_dispatched_total", "help text", 42, 1.0);
    assert!(
        out.contains("# HELP messages_dispatched help text\n"),
        "HELP must use base name without _total: {out}"
    );
    assert!(
        out.contains("# TYPE messages_dispatched counter\n"),
        "TYPE must use base name without _total: {out}"
    );
    assert!(
        out.contains("messages_dispatched_total 42\n"),
        "value line must keep _total suffix: {out}"
    );
    assert!(
        out.contains("messages_dispatched_created 1\n"),
        "_created must use base name: {out}"
    );
}

#[test]
fn render_counter_passthrough_when_no_total_suffix() {
    let mut out = String::new();
    render_counter(&mut out, "my_counter", "desc", 7, 2.0);
    assert!(out.contains("# HELP my_counter desc\n"));
    assert!(out.contains("my_counter 7\n"));
    assert!(out.contains("my_counter_created 2\n"));
}

// ── render_histogram ──────────────────────────────────────────────────────

/// Histogram buckets must be **cumulative** (each bucket includes all
/// lower-boundary observations). This is the load-bearing invariant for
/// correct Prometheus rate/quantile calculations.
#[test]
fn render_histogram_produces_cumulative_bucket_counts() {
    use crate::hub::LatencyHistogram;
    use std::sync::atomic::Ordering;

    let h = LatencyHistogram::new();
    // Bucket layout: [1, 5, 25, 100, 500, 2500, 10000]
    // Observe one event in bucket[0] (≤ 1ms) and one in bucket[1] (≤ 5ms).
    h.buckets[0].store(3, Ordering::Relaxed);
    h.buckets[1].store(2, Ordering::Relaxed);
    h.count.store(5, Ordering::Relaxed);

    let mut out = String::new();
    render_histogram(&mut out, "test_latency", "desc", &h, 0.0);

    // bucket le="1" must be 3 (not cumulative yet, first bucket)
    assert!(
        out.contains("test_latency_bucket{le=\"1\"} 3\n"),
        "first bucket must equal bucket[0] count: {out}"
    );
    // bucket le="5" must be 5 (3 from bucket[0] + 2 from bucket[1]) — cumulative!
    assert!(
        out.contains("test_latency_bucket{le=\"5\"} 5\n"),
        "second bucket must be cumulative (3+2=5): {out}"
    );
    // +Inf must include all counts
    assert!(
        out.contains("test_latency_bucket{le=\"+Inf\"} 5\n"),
        "+Inf bucket must equal total count: {out}"
    );
}

#[test]
fn render_histogram_includes_help_and_type_lines() {
    use crate::hub::LatencyHistogram;
    let h = LatencyHistogram::new();
    let mut out = String::new();
    render_histogram(&mut out, "my_latency", "my help", &h, 0.0);
    assert!(out.contains("# HELP my_latency my help\n"));
    assert!(out.contains("# TYPE my_latency histogram\n"));
}
