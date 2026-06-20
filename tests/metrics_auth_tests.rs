use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use ilink_hub::{
    hub::HubState, ilink::UpstreamClient, server::build_router, store::Store, InMemoryQueue,
};
use tower::ServiceExt; // for .oneshot()

const METRICS_TEST_TOKEN: &str = "test-metrics-admin-token";

static ENV_INSTALLED: std::sync::Once = std::sync::Once::new();

fn install_test_env() {
    ENV_INSTALLED.call_once(|| unsafe {
        std::env::set_var("ILINK_ADMIN_TOKEN", METRICS_TEST_TOKEN);
    });
}

async fn make_state() -> Arc<HubState> {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
    )
}

#[tokio::test]
async fn test_metrics_auth_with_configured_token() {
    install_test_env();

    let state = make_state().await;
    let app = build_router(state);

    // 1. No auth header -> 401 Unauthorized
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 2. Wrong auth token -> 401 Unauthorized
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .header("Authorization", "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 3. Correct auth token -> 200 OK
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .header("Authorization", format!("Bearer {METRICS_TEST_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
