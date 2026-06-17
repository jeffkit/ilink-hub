pub mod pairing;
pub mod routes;

use axum::{
    extract::DefaultBodyLimit,
    http::HeaderValue,
    routing::{get, patch, post},
    Router,
};
use routes::{admin_ilink_qr_stream, admin_ilink_relogin, admin_ilink_status};
use std::{env, sync::Arc};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::hub::HubState;
use pairing::*;
use routes::*;

fn parse_origins(raw: &str) -> Vec<HeaderValue> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if !s.contains("://") {
                panic!("ILINK_CORS_ORIGINS contains origin without scheme: {s}");
            }
            HeaderValue::from_str(s)
                .unwrap_or_else(|_| panic!("ILINK_CORS_ORIGINS contains invalid origin: {s}"))
        })
        .collect()
}

/// Build CORS layer from `ILINK_CORS_ORIGINS` env var (comma-separated origins).
/// Falls back to permissive CORS when the env var is absent or empty.
fn build_cors_layer() -> CorsLayer {
    match env::var("ILINK_CORS_ORIGINS") {
        Ok(ref val) if !val.trim().is_empty() => {
            let origins = parse_origins(val);
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods(Any)
                .allow_headers(Any)
        }
        _ => {
            tracing::warn!("ILINK_CORS_ORIGINS not set or empty, falling back to permissive CORS");
            CorsLayer::permissive()
        }
    }
}

pub fn build_router(state: Arc<HubState>) -> Router {
    // CORS is only required for the iLink-compatible bot API so that browser-based
    // SDK clients (e.g. OpenClaw) can call it from any origin.
    // Hub management and admin routes deliberately do NOT get CORS headers — they
    // should only be called server-side or via same-origin UI.
    let bot_cors = build_cors_layer();

    let bot_api = Router::new()
        .route(
            "/ilink/bot/get_bot_qrcode",
            get(get_bot_qrcode).post(get_bot_qrcode_post),
        )
        .route("/ilink/bot/get_qrcode_status", get(get_qrcode_status))
        .route("/ilink/bot/getupdates", post(getupdates))
        .route(
            "/ilink/bot/sendmessage",
            post(sendmessage).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route("/ilink/bot/sendtyping", post(sendtyping))
        .route("/ilink/bot/getconfig", post(getconfig))
        .route("/ilink/bot/getuploadurl", post(getuploadurl))
        .layer(bot_cors);

    let admin_api = Router::new()
        // Hub management (non-iLink)
        .route("/hub/register", post(register))
        .route("/hub/clients", get(admin_clients))
        .route(
            "/hub/clients/{name}",
            patch(admin_update_client).delete(admin_delete_client),
        )
        .route("/hub/ui", get(admin_ui))
        .route("/hub/pair/{code}", get(pair_page))
        .route("/hub/pair/{code}/confirm", post(pair_confirm))
        // iLink upstream management
        .route("/hub/ilink/status", get(admin_ilink_status))
        .route("/hub/ilink/relogin", post(admin_ilink_relogin))
        .route("/hub/ilink/qr-stream", get(admin_ilink_qr_stream))
        // Observability
        .route("/metrics", get(metrics))
        .route("/health", get(|| async { "ok" }));

    Router::new()
        .merge(bot_api)
        .merge(admin_api)
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    // ── parse_origins unit tests ──────────────────────────────────────

    #[test]
    fn origins_single() {
        let origins = parse_origins("https://example.com");
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0], HeaderValue::from_static("https://example.com"));
    }

    #[test]
    fn origins_multiple() {
        let origins = parse_origins("https://a.com, https://b.com");
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0], HeaderValue::from_static("https://a.com"));
        assert_eq!(origins[1], HeaderValue::from_static("https://b.com"));
    }

    #[test]
    fn origins_trims_whitespace() {
        let origins = parse_origins(" https://a.com ,  https://b.com ");
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0], HeaderValue::from_static("https://a.com"));
        assert_eq!(origins[1], HeaderValue::from_static("https://b.com"));
    }

    #[test]
    fn origins_empty_string() {
        let origins = parse_origins("");
        assert!(origins.is_empty());
    }

    #[test]
    fn origins_only_whitespace() {
        let origins = parse_origins("   ,  ");
        assert!(origins.is_empty());
    }

    #[test]
    #[should_panic(expected = "without scheme")]
    fn origins_rejects_control_chars() {
        parse_origins("bad\norigin");
    }

    // ── M2: boundary handling ─────────────────────────────────────────

    #[test]
    #[should_panic(expected = "without scheme")]
    fn origins_rejects_no_scheme() {
        parse_origins("bad-origin");
    }

    #[test]
    #[should_panic(expected = "without scheme")]
    fn origins_rejects_wildcard() {
        parse_origins("*");
    }

    #[test]
    #[should_panic(expected = "without scheme")]
    fn origins_rejects_null_origin() {
        parse_origins("null");
    }

    #[test]
    fn origins_duplicates_are_preserved() {
        let origins = parse_origins("https://a.com, https://a.com, https://b.com");
        assert_eq!(origins.len(), 3);
    }

    #[test]
    fn origins_trailing_comma() {
        let origins = parse_origins("https://a.com,");
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0], HeaderValue::from_static("https://a.com"));
    }

    #[test]
    #[should_panic(expected = "without scheme")]
    fn origins_rejects_mixed_with_bad_origin() {
        // Even if some origins are valid, a single bad one should fail fast.
        parse_origins("https://a.com, bad-origin, https://b.com");
    }

    // ── build_cors_layer integration tests ────────────────────────────

    /// Build a minimal router with the CORS layer so we can send synthetic
    /// HTTP requests and inspect CORS response headers.
    fn test_router(cors: CorsLayer) -> axum::Router {
        axum::Router::new()
            .route("/ping", axum::routing::post(|| async { "pong" }))
            .layer(cors)
    }

    #[test]
    fn permissive_allows_any_origin() {
        // Explicitly unset ILINK_CORS_ORIGINS via temp_env to prevent leakage
        // from parallel tests that set it.
        temp_env::with_var("ILINK_CORS_ORIGINS", None::<&str>, || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let cors = build_cors_layer(); // no env → permissive
                let app = test_router(cors);

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
                let cors = build_cors_layer(); // no env → permissive
                let app = test_router(cors);

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
                    "preflight must include Access-Control-Allow-Methods, got: {:?}",
                    resp.headers()
                );
            });
        });
    }

    #[test]
    fn list_mode_allows_configured_origin() {
        temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let cors = build_cors_layer();
                let app = test_router(cors);

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
    fn list_mode_preflight_includes_allow_methods_and_headers() {
        temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let cors = build_cors_layer();
                let app = test_router(cors);

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

                assert!(
                    resp.status().is_success(),
                    "preflight should succeed, got {}",
                    resp.status()
                );
                let am = resp
                    .headers()
                    .get("access-control-allow-methods")
                    .expect("preflight must include Access-Control-Allow-Methods");
                // Any returns "*" (all methods allowed)
                assert_eq!(am, "*", "allow-methods should be *, got: {:?}", am);
                let ah = resp
                    .headers()
                    .get("access-control-allow-headers")
                    .expect("preflight must include Access-Control-Allow-Headers");
                // Any returns "*" (all headers allowed)
                assert_eq!(ah, "*", "allow-headers should be *, got: {:?}", ah);
            });
        });
    }

    #[test]
    fn list_mode_rejects_unlisted_origin() {
        temp_env::with_var("ILINK_CORS_ORIGINS", Some("https://my.app"), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let cors = build_cors_layer();
                let app = test_router(cors);

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
                let cors = build_cors_layer();
                let app = test_router(cors);

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

                // The critical security property: no allow-origin means the browser
                // will reject the response and never send the actual request.
                assert!(
                    resp.headers().get("access-control-allow-origin").is_none(),
                    "preflight for unlisted origin should not return allow-origin"
                );
            });
        });
    }

    #[tokio::test]
    async fn cors_layer_is_cloneable_for_tower() {
        // Ensure CorsLayer implements Clone so tower can distribute it
        // across worker tasks without shared mutable state.
        let cors = build_cors_layer();
        let _c2 = cors.clone();
    }
}
