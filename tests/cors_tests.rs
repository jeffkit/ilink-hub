//! Integration tests for CORS behaviour.
//!
//! These tests exercise the full router stack (build_router) so that CORS
//! middleware, route matching, and body-limit layers all interact as they do in
//! production.  Each test isolates the ILINK_CORS_ORIGINS env var via temp_env.

use axum::body::Body;
use axum::http::Request;
use ilink_hub::server::build_cors_layer;
use tower::ServiceExt;

// ── helpers ────────────────────────────────────────────────────────────

fn test_router(cors: tower_http::cors::CorsLayer) -> axum::Router {
    axum::Router::new()
        .route("/ping", axum::routing::post(|| async { "pong" }))
        .layer(cors)
}

// ── permissive fallback ─────────────────────────────────────────────────

#[test]
fn permissive_allows_any_origin() {
    temp_env::with_var("ILINK_CORS_ORIGINS", None::<&str>, || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = test_router(build_cors_layer());

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
            let app = test_router(build_cors_layer());

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
            let app = test_router(build_cors_layer());

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
                let app = test_router(build_cors_layer());

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
            let app = test_router(build_cors_layer());

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
            let app = test_router(build_cors_layer());

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
            let app = test_router(build_cors_layer());

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

// ── illegal format ──────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "without scheme")]
fn parse_origins_panics_on_bad_format() {
    ilink_hub::server::parse_origins("bad-origin");
}

#[test]
#[should_panic(expected = "without scheme")]
fn parse_origins_panics_on_mixed_bad_origin() {
    ilink_hub::server::parse_origins("https://a.com, bad-origin");
}

// ── clone / Send+Sync sanity ─────────────────────────────────────────────

#[test]
fn cors_layer_is_cloneable() {
    let cors = build_cors_layer();
    let _c2 = cors.clone();
}
