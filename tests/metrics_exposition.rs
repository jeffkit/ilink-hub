//! Regressions for issue #36 — hand-written exposition blocks in the `/metrics`
//! handler (`src/server/routes/metrics.rs`): a `client="unknown"` series used to
//! be emitted once per unnamed vtoken (duplicate timeseries ⇒ Prometheus rejects
//! the whole scrape), and `ilink_hub_messages_rejected_total` used to
//! interpolate the client name without escaping.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ilink_hub::hub::{push_to_queue_pub, AdminConfig, HubState};
use ilink_hub::ilink::types::WeixinMessage;
use ilink_hub::ilink::UpstreamClient;
use ilink_hub::server::build_router;
use ilink_hub::store::Store;
use ilink_hub::InMemoryQueue;
use tower::ServiceExt; // for `.oneshot()`

const ADMIN_TOKEN: &str = "test-issue36-admin-token";

fn admin_config() -> AdminConfig {
    AdminConfig {
        token: Some(ADMIN_TOKEN.to_string()),
        insecure_no_auth: false,
        outbound_origin_label: None,
    }
}

/// Queue limit of 1: the second push for a vtoken takes the product
/// backpressure path (`push_to_queue_pub` → `messages_rejected_by_client`).
async fn make_state() -> Arc<HubState> {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let queue = Arc::new(InMemoryQueue::with_limit(1));
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        admin_config(),
    )
}

fn msg(text: &str) -> WeixinMessage {
    WeixinMessage {
        from_user_id: Some(text.to_string()),
        ..Default::default()
    }
}

async fn register_client(state: &Arc<HubState>, name: &str) -> String {
    let (_, vtoken, is_new) =
        state
            .clients
            .registry
            .write()
            .await
            .register(name.to_string(), None, None);
    assert!(is_new, "fresh registration expected for {name:?}");
    vtoken
}

/// Seed one unacknowledged queue entry plus one backpressure rejection for a
/// client, delete the client through the product path, then push once more —
/// modelling the in-flight dispatch that lands *after* the delete drained the
/// queue slot (the state issue #36 describes: a vtoken with a queue entry but no
/// name mapping).
async fn seed_deleted_client(state: &Arc<HubState>, name: &str) -> String {
    let vtoken = register_client(state, name).await;
    push_to_queue_pub(&state.clients.queue, &state.metrics, &vtoken, msg("first")).await;
    push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &vtoken,
        msg("overflow"),
    )
    .await;
    ilink_hub::server::pairing::unregister_client_in_hub(state.as_ref(), name, true)
        .await
        .expect("unregister");
    push_to_queue_pub(&state.clients.queue, &state.metrics, &vtoken, msg("late")).await;
    vtoken
}

async fn scrape(state: &Arc<HubState>) -> String {
    let app = build_router(Arc::clone(state));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 exposition")
}

fn lines_starting_with<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.starts_with(prefix)).collect()
}

// ─── Minimal Prometheus text-format parser (stand-in for `promtool`) ─────────
//
// Parses every sample line into `(metric_name, sorted labels)` and panics on
// malformed exposition — an unescaped newline inside a label value splits one
// sample into two unparseable lines, which is how the escaping bug surfaces.
// Only `\\`, `\"` and `\n` are valid escapes in the text format, so any other
// backslash sequence is reported as invalid.

fn parse_samples(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut samples = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, value) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("sample line has no value: {line:?}"));
        assert!(
            value.parse::<f64>().is_ok(),
            "sample value is not a number: {line:?}"
        );
        samples.push(parse_head(head));
    }
    samples
}

fn parse_head(head: &str) -> (String, Vec<(String, String)>) {
    let Some((name, rest)) = head.split_once('{') else {
        return (head.to_string(), Vec::new());
    };
    let inner = rest
        .strip_suffix('}')
        .unwrap_or_else(|| panic!("label list not terminated: {head:?}"));
    let mut labels = Vec::new();
    let mut chars = inner.chars().peekable();
    while chars.peek().is_some() {
        let mut label_name = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            label_name.push(c);
            chars.next();
        }
        assert_eq!(chars.next(), Some('='), "expected `=` in {head:?}");
        assert_eq!(
            chars.next(),
            Some('"'),
            "expected a quoted label value in {head:?}"
        );
        let mut label_value = String::new();
        loop {
            match chars.next() {
                None => panic!("unterminated label value in {head:?}"),
                Some('\\') => match chars.next() {
                    Some('\\') => label_value.push('\\'),
                    Some('"') => label_value.push('"'),
                    Some('n') => label_value.push('\n'),
                    other => panic!("invalid escape {other:?} in {head:?}"),
                },
                Some('"') => break,
                Some(c) => label_value.push(c),
            }
        }
        labels.push((label_name, label_value));
        match chars.next() {
            None => break,
            Some(',') => continue,
            other => panic!("unexpected {other:?} after a label value in {head:?}"),
        }
    }
    labels.sort();
    (name.to_string(), labels)
}

fn assert_no_duplicate_timeseries(text: &str) {
    let mut seen = std::collections::HashSet::new();
    for (name, labels) in parse_samples(text) {
        assert!(
            seen.insert((name.clone(), labels.clone())),
            "duplicate timeseries {name}{labels:?} — Prometheus rejects the entire scrape"
        );
    }
}

// ─── Cases ───────────────────────────────────────────────────────────────────

/// Two deleted clients (no name mapping) that each still hold a queue entry born
/// of the same two hand-written blocks must not produce two series with the
/// identical label set.
#[tokio::test]
async fn unknown_vtoken_series_are_collapsed_to_one() {
    let state = make_state().await;
    seed_deleted_client(&state, "gone-a").await;
    seed_deleted_client(&state, "gone-b").await;

    let text = scrape(&state).await;

    let queue_unknown = lines_starting_with(&text, "ilink_hub_queue_size{client=\"unknown\"}");
    assert_eq!(
        queue_unknown.len(),
        1,
        "expected exactly one `client=\"unknown\"` queue_size series, got:\n{queue_unknown:#?}\n---\n{text}"
    );
    // Collapsed, not dropped: each deleted client still holds one unacked entry.
    assert_eq!(
        queue_unknown[0],
        "ilink_hub_queue_size{client=\"unknown\"} 2"
    );

    let rejected_unknown = lines_starting_with(
        &text,
        "ilink_hub_messages_rejected_total{client=\"unknown\"}",
    );
    assert_eq!(
        rejected_unknown.len(),
        1,
        "expected exactly one `client=\"unknown\"` messages_rejected_total series, got:\n{rejected_unknown:#?}\n---\n{text}"
    );
    // Each deleted client took exactly one backpressure rejection.
    assert_eq!(
        rejected_unknown[0],
        "ilink_hub_messages_rejected_total{client=\"unknown\"} 2"
    );
}

/// A client whose name is the literal string `unknown` shares the unnamed
/// bucket, so the family still holds exactly one series with that label set.
#[tokio::test]
async fn literal_unknown_name_shares_the_unnamed_bucket() {
    let state = make_state().await;
    let vtoken = register_client(&state, "unknown").await;
    push_to_queue_pub(&state.clients.queue, &state.metrics, &vtoken, msg("first")).await;
    push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &vtoken,
        msg("overflow"),
    )
    .await;
    seed_deleted_client(&state, "gone-a").await;

    let text = scrape(&state).await;

    let queue_unknown = lines_starting_with(&text, "ilink_hub_queue_size{client=\"unknown\"}");
    assert_eq!(queue_unknown.len(), 1, "exposition:\n{text}");
    assert_eq!(
        queue_unknown[0],
        "ilink_hub_queue_size{client=\"unknown\"} 2"
    );

    let rejected_unknown = lines_starting_with(
        &text,
        "ilink_hub_messages_rejected_total{client=\"unknown\"}",
    );
    assert_eq!(rejected_unknown.len(), 1, "exposition:\n{text}");
    assert_eq!(
        rejected_unknown[0],
        "ilink_hub_messages_rejected_total{client=\"unknown\"} 2"
    );

    assert_no_duplicate_timeseries(&text);
}

/// A client name containing `"`, `\` and a newline must be escaped in *both*
/// hand-written blocks and each sample must stay on a single line.
#[tokio::test]
async fn client_name_is_escaped_in_both_hand_written_blocks() {
    const NAME: &str = "evil\"name\\with\nnewline";
    let state = make_state().await;
    let vtoken = register_client(&state, NAME).await;
    push_to_queue_pub(&state.clients.queue, &state.metrics, &vtoken, msg("first")).await;
    push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &vtoken,
        msg("overflow"),
    )
    .await;

    let text = scrape(&state).await;

    let escaped = r#"evil\"name\\with\nnewline"#;
    let expected_queue = format!("ilink_hub_queue_size{{client=\"{escaped}\"}} 1");
    let queue_lines = lines_starting_with(&text, "ilink_hub_queue_size{");
    assert_eq!(queue_lines.len(), 1, "exposition:\n{text}");
    assert_eq!(queue_lines[0], expected_queue);

    let expected_rejected = format!("ilink_hub_messages_rejected_total{{client=\"{escaped}\"}} 1");
    let rejected_lines = lines_starting_with(&text, "ilink_hub_messages_rejected_total");
    assert_eq!(rejected_lines.len(), 1, "exposition:\n{text}");
    assert_eq!(rejected_lines[0], expected_rejected);
}

/// Whole-endpoint parse: no duplicated timeseries, no malformed or invalid
/// exposition (the equivalent of `promtool check metrics`).
#[tokio::test]
async fn exposition_parses_without_duplicates_or_invalid_lines() {
    let state = make_state().await;
    seed_deleted_client(&state, "gone-a").await;
    seed_deleted_client(&state, "gone-b").await;
    register_client(&state, "named-client").await;

    let text = scrape(&state).await;
    assert_no_duplicate_timeseries(&text);
}
