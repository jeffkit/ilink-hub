//! Integration tests for CORS behaviour.
//!
//! These tests exercise the full router stack (build_router) so that CORS
//! middleware, route matching, and body-limit layers all interact as they do in
//! production.  Each test isolates the ILINK_CORS_ORIGINS env var via temp_env.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use ilink_hub::hub::{AdminConfig, HubState};
use ilink_hub::ilink::UpstreamClient;
use ilink_hub::server::{build_cors_layer, build_router};
use ilink_hub::store::Store;
use ilink_hub::InMemoryQueue;
use tower::ServiceExt;

// ── helpers ────────────────────────────────────────────────────────────

fn test_router(cors: tower_http::cors::CorsLayer) -> axum::Router {
    axum::Router::new()
        .route("/ping", axum::routing::post(|| async { "pong" }))
        .layer(cors)
}

async fn make_hub_state() -> Arc<HubState> {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
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

// ── permissive fallback ─────────────────────────────────────────────────

#[test]
fn permissive_allows_any_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", None::<&str>, || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ping")
                        .header("Origin", "https://evil.example.com")
                        .header("Content-Type", "application/json")
                        .body(Body::from(r#"{"k":"v"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), 200);
            assert_eq!(
                resp.headers().get("access-control-allow-origin").unwrap(),
                "*"
            );
        });
    });
}

#[test]
fn permissive_preflight_returns_allow_methods() {
    temp_env::with_var("ILINK_CORS_ORIGINS", None::<&str>, || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/ping")
                        .header("Origin", "https://evil.example.com")
                        .header("Access-Control-Request-Method", "POST")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert!(resp.status().is_success(), "preflight should succeed");
            assert!(
                resp.headers().contains_key("access-control-allow-methods"),
                "preflight must include Access-Control-Allow-Methods"
            );
        });
    });
}

// ── list mode: allow ────────────────────────────────────────────────────

#[test]
fn list_mode_allows_configured_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ping")
                        .header("Origin", "https://my.app")
                        .header("Content-Type", "application/json")
                        .body(Body::from(r#"{}"#))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), 200);
            assert_eq!(
                resp.headers().get("access-control-allow-origin").unwrap(),
                "https://my.app"
            );
        });
    });
}

#[test]
fn list_mode_allows_multiple_origins() {
    temp_env::with_var(
        "ILINK_CORS_ORIGINS",
        Some("https://a.com, https://b.com"),
        || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let app = test_router(build_cors_layer().unwrap());

                for origin in &["https://a.com", "https://b.com"] {
                    let resp = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri("/ping")
                                .header("Origin", *origin)
                                .header("Content-Type", "application/json")
                                .body(Body::from(r#"{}"#))
                                .unwrap(),
                        )
                        .await
                        .unwrap();

                    assert_eq!(resp.status(), 200);
                    assert_eq!(
                        resp.headers().get("access-control-allow-origin").unwrap(),
                        *origin
                    );
                }
            });
        },
    );
}

#[test]
fn list_mode_preflight_includes_allow_methods_and_headers() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/ping")
                        .header("Origin", "https://my.app")
                        .header("Access-Control-Request-Method", "POST")
                        .header("Access-Control-Request-Headers", "content-type")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert!(resp.status().is_success());
            assert_eq!(
                resp.headers()
                    .get("access-control-allow-methods")
                    .expect("preflight must include Access-Control-Allow-Methods"),
                "*"
            );
            assert_eq!(
                resp.headers()
                    .get("access-control-allow-headers")
                    .expect("preflight must include Access-Control-Allow-Headers"),
                "*"
            );
        });
    });
}

// ── list mode: reject ────────────────────────────────────────────────────

#[test]
fn list_mode_rejects_unlisted_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ping")
                        .header("Origin", "https://evil.example.com")
                        .header("Content-Type", "application/json")
                        .body(Body::from(r#"{}"#))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                resp.headers().get("access-control-allow-origin"),
                None,
                "unlisted origin should NOT receive allow-origin header"
            );
        });
    });
}

#[test]
fn list_mode_preflight_rejects_unlisted_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer().unwrap());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/ping")
                        .header("Origin", "https://evil.example.com")
                        .header("Access-Control-Request-Method", "POST")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert!(
                resp.headers().get("access-control-allow-origin").is_none(),
                "preflight for unlisted origin should not return allow-origin"
            );
        });
    });
}

// ── production build_router path ─────────────────────────────────────────

/// Regression: `build_router` must wire `build_cors_layer()`, not a hard-coded
/// `CorsLayer::permissive()`. With a whitelist set, an evil Origin must not
/// receive `Access-Control-Allow-Origin: *` (and must not be echoed).
#[test]
fn build_router_whitelist_rejects_evil_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let state = make_hub_state().await;
            let app = build_router(state);

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/health")
                        .header("Origin", "https://evil.example.com")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            // /health is on admin_api (no CORS layer). Even if bot CORS were
            // permissive, admin routes must not advertise *. The critical
            // assertion for M1 is that production wiring is not permissive
            // on the bot API — exercise that next.
            let acao = resp.headers().get("access-control-allow-origin");
            let acao_bytes = acao.map(|v| v.as_bytes());
            assert!(
                acao_bytes.is_none() || acao_bytes != Some(b"*".as_slice()),
                "must not advertise Access-Control-Allow-Origin: *; got {acao:?}"
            );
            assert_ne!(
                acao_bytes,
                Some(b"https://evil.example.com".as_slice()),
                "must not echo the evil Origin"
            );
        });
    });
}

/// Bot API path through `build_router`: whitelist mode must not allow evil Origin.
#[test]
fn build_router_bot_api_whitelist_rejects_evil_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let state = make_hub_state().await;
            let app = build_router(state);

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/ilink/bot/get_bot_qrcode?bot_type=3")
                        .header("Origin", "https://evil.example.com")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            let acao = resp.headers().get("access-control-allow-origin");
            let acao_bytes = acao.map(|v| v.as_bytes());
            assert_ne!(
                acao_bytes,
                Some(b"*".as_slice()),
                "build_router must use build_cors_layer(); whitelist must not yield *"
            );
            assert!(
                acao_bytes.is_none() || acao_bytes != Some(b"https://evil.example.com".as_slice()),
                "evil Origin must not be echoed; got {acao:?}"
            );
        });
    });
}

/// Bot API path through `build_router`: configured origin is allowed.
#[test]
fn build_router_bot_api_whitelist_allows_configured_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let state = make_hub_state().await;
            let app = build_router(state);

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/ilink/bot/get_bot_qrcode?bot_type=3")
                        .header("Origin", "https://my.app")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                resp.headers()
                    .get("access-control-allow-origin")
                    .map(|v| v.as_bytes()),
                Some(b"https://my.app".as_slice()),
                "configured origin must be allowed on bot API via build_router"
            );
        });
    });
}

// ── illegal format ──────────────────────────────────────────────────────

#[test]
fn parse_origins_errs_on_bad_format() {
    let err = ilink_hub::server::parse_origins("bad-origin").unwrap_err();
    assert!(err.contains("http://") || err.contains("https://"), "{err}");
}

#[test]
fn parse_origins_errs_on_mixed_bad_origin() {
    let err = ilink_hub::server::parse_origins("https://a.com, bad-origin").unwrap_err();
    assert!(err.contains("bad-origin"), "{err}");
}

// ── clone / Send+Sync sanity ─────────────────────────────────────────────

#[test]
fn cors_layer_is_cloneable() {
    let cors = build_cors_layer().unwrap();
    let _c2 = cors.clone();
}
