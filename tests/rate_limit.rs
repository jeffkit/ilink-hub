//! Per-vtoken outbound rate limiting (issue #29).
//!
//! Every tenant of the Hub shares one real WeChat account and one upstream
//! connection pool, so the upstream quota is global even though the virtual
//! tokens are per-tenant. Before this change the outbound surface was
//! protected only in dimensions that do not isolate tenants: `sendmessage` had
//! one Hub-wide 64-slot concurrency cap, and `sendtyping` / `getconfig` /
//! `getuploadurl` had no gate at all. A single tenant (or a leaked token)
//! could therefore drive the shared account's quota without limit.
//!
//! These tests pin the fix's observable contract:
//!
//! 1. A tenant's outbound calls are capped by a token bucket (burst, then 429).
//! 2. The bucket is shared across every outbound surface — `sendmessage`,
//!    `sendtyping`, and the MCP `call_agent` tool all draw on the same quota,
//!    so a tenant cannot bypass its limit by switching routes.
//! 3. Buckets are per-tenant: one tenant exhausting its quota does not affect
//!    another tenant (the noisy-neighbour property).
//! 4. The rejection and the remaining quota are visible per-tenant at
//!    `/metrics`.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use ilink_hub::{
    hub::{AdminConfig, HubState},
    ilink::UpstreamClient,
    server::build_router,
    store::Store,
    InMemoryQueue,
};
use tower::ServiceExt; // for .oneshot()

const ADMIN_TOKEN: &str = "rate-limit-test-admin-token";

static ENV_INSTALLED: std::sync::Once = std::sync::Once::new();

fn install_test_env() {
    ENV_INSTALLED.call_once(|| unsafe {
        std::env::set_var("ILINK_ADMIN_TOKEN", ADMIN_TOKEN);
    });
}

/// A Hub whose upstream is a local mock that accepts every outbound call, so
/// the only thing that can reject a request in these tests is the rate limiter
/// (or, where noted, ordinary request validation).
async fn make_state() -> Arc<HubState> {
    install_test_env();

    let mock_app = Router::new()
        .route("/ilink/bot/sendtyping", post(|| async { StatusCode::OK }))
        .route("/ilink/bot/sendmessage", post(|| async { StatusCode::OK }))
        .route("/ilink/bot/getconfig", post(|| async { StatusCode::OK }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream = Arc::new(
        UpstreamClient::new("sk-test:key".to_string(), Some(format!("http://{addr}")))
            .expect("test upstream client"),
    );
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    )
}

/// Register a client and return its plaintext vtoken — what a real bridge puts
/// in `Authorization: Bearer` (the Hub hashes it on receipt).
async fn register(state: &Arc<HubState>, name: &str) -> String {
    let outcome =
        ilink_hub::server::pairing::register_client_in_hub(state, name.to_string(), None, None)
            .await;
    assert!(
        !outcome.plaintext.is_empty(),
        "registration must mint a plaintext token for '{name}'"
    );
    outcome.plaintext
}

/// Pin the bucket far from wall-clock sensitivity: `burst` tokens and a refill
/// rate slow enough that no token can come back during a test run.
fn freeze_bucket(state: &HubState, burst: f64) {
    state.clients.rate_limiter.set_limits(1e-6, burst);
}

async fn post_json(
    app: &Router,
    uri: &str,
    vtoken: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", format!("Bearer {vtoken}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn send_typing(app: &Router, vtoken: &str) -> (StatusCode, serde_json::Value) {
    post_json(
        app,
        "/ilink/bot/sendtyping",
        vtoken,
        r#"{"vctx":"vctx-123","typing":true}"#,
    )
    .await
}

/// `sendmessage` with a body that has no `msg.context_token`. The rate-limit
/// gate runs *before* context validation, so the two outcomes are
/// distinguishable: `ret: 400` means the request got past the gate (and was
/// then rejected for a missing context, as expected), while HTTP 429 means the
/// gate itself stopped it.
async fn send_message(app: &Router, vtoken: &str) -> (StatusCode, serde_json::Value) {
    post_json(app, "/ilink/bot/sendmessage", vtoken, r#"{"msg":{}}"#).await
}

async fn mcp_call_agent(app: &Router, vtoken: &str) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "call_agent",
            "arguments": { "name": "someone-else", "message": "hi" }
        }
    })
    .to_string();
    let (_, json) = post_json(app, "/mcp", vtoken, &body).await;
    json
}

async fn fetch_metrics(app: &Router) -> String {
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/metrics must succeed");
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn tenant_is_capped_at_its_burst_then_gets_429() {
    let state = make_state().await;
    let vtoken = register(&state, "burst-client").await;
    freeze_bucket(&state, 3.0);
    let app = build_router(state);

    for i in 0..3 {
        let (status, body) = send_typing(&app, &vtoken).await;
        assert_eq!(status, StatusCode::OK, "call {i} is within the burst");
        assert_eq!(body["ret"], 0, "call {i} must reach the upstream");
    }

    let (status, body) = send_typing(&app, &vtoken).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the request past the burst must carry HTTP 429"
    );
    assert_eq!(body["ret"], 429);
    assert!(
        body["errmsg"]
            .as_str()
            .unwrap_or_default()
            .contains("rate limit exceeded"),
        "the body must explain the rejection, got {body}"
    );
}

#[tokio::test]
async fn all_outbound_surfaces_share_one_bucket() {
    let state = make_state().await;
    let vtoken = register(&state, "shared-bucket-client").await;
    freeze_bucket(&state, 2.0);
    let app = build_router(state);

    // Call 1 draws a token via `sendtyping`.
    let (status, body) = send_typing(&app, &vtoken).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ret"], 0);

    // Call 2 draws the second token via `sendmessage` and is admitted by the
    // gate — it then fails context validation, which is the observable proof
    // that the limiter let it through rather than stopping it at 429.
    let (status, body) = send_message(&app, &vtoken).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["ret"], 400,
        "the second call must get past the rate gate and fail on its own merits"
    );

    // Call 3: the bucket is empty, so every surface is now closed. If the
    // bucket were per-route, `sendtyping` would still have quota left here.
    let (status, _) = send_typing(&app, &vtoken).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "sendtyping must see the quota that sendmessage consumed"
    );

    let (status, body) = send_message(&app, &vtoken).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["ret"], 429);

    // The MCP A2A path also ends in an outbound send, so it must be gated too —
    // otherwise `call_agent` would be a trivial bypass.
    let mcp = mcp_call_agent(&app, &vtoken).await;
    let err = mcp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("rate limit exceeded"),
        "MCP call_agent must be gated by the same bucket, got {mcp}"
    );
}

#[tokio::test]
async fn one_tenant_exhausting_its_bucket_does_not_throttle_another() {
    let state = make_state().await;
    let noisy = register(&state, "noisy-client").await;
    let quiet = register(&state, "quiet-client").await;
    freeze_bucket(&state, 1.0);
    let app = build_router(state);

    assert_eq!(send_typing(&app, &noisy).await.0, StatusCode::OK);
    assert_eq!(
        send_typing(&app, &noisy).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the noisy tenant is out of quota"
    );

    // The whole point of the issue: the quiet tenant must be unaffected.
    let (status, body) = send_typing(&app, &quiet).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a noisy neighbour must not consume another tenant's quota"
    );
    assert_eq!(body["ret"], 0);
}

#[tokio::test]
async fn unauthenticated_requests_never_allocate_a_bucket() {
    let state = make_state().await;
    freeze_bucket(&state, 1.0);
    let app = build_router(state.clone());

    // An unknown token is rejected by auth, and auth runs *before* the limiter,
    // so an anonymous flood cannot grow (or thrash) the limiter's bucket map.
    for _ in 0..5 {
        let (status, body) = send_typing(&app, "vhub_00000000000000000000000000000000").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "auth failures keep the legacy shape"
        );
        assert_eq!(body["ret"], 401);
    }

    assert_eq!(
        state.clients.rate_limiter.tracked_count(),
        0,
        "rejected-unauthenticated requests must not allocate a bucket"
    );
}

#[tokio::test]
async fn rejection_and_remaining_quota_are_visible_per_tenant() {
    let state = make_state().await;
    let vtoken = register(&state, "metrics-client").await;
    freeze_bucket(&state, 1.0);
    let app = build_router(state);

    // One token: the first call spends it, the second is rejected.
    assert_eq!(send_typing(&app, &vtoken).await.0, StatusCode::OK);
    assert_eq!(
        send_typing(&app, &vtoken).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let text = fetch_metrics(&app).await;
    assert!(
        text.contains(r#"ilink_hub_ratelimit_tokens{client="metrics-client"} 0"#),
        "remaining quota must be exported per tenant:\n{text}"
    );
    assert!(
        text.contains(r#"ilink_hub_ratelimit_burst{client="metrics-client"} 1"#),
        "the configured burst must be exported per tenant:\n{text}"
    );
    assert!(
        text.contains(r#"ilink_hub_ratelimit_rejected_total{client="metrics-client"} 1"#),
        "the per-tenant 429 count must be exported:\n{text}"
    );
}
