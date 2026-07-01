pub mod pairing;
pub mod routes;
pub mod sse_ticket;

use axum::http::HeaderValue;
use axum::{
    extract::{DefaultBodyLimit, Request},
    middleware::{self, Next},
    response::Response,
    routing::{get, patch, post},
    Router,
};
use routes::{
    admin_client_session_history, admin_client_sessions, admin_ilink_qr_stream,
    admin_ilink_qr_stream_ticket, admin_ilink_relogin, admin_ilink_status,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Parse a comma-separated list of allowed origins from a string.
///
/// Each entry must include a scheme (`https://` or `http://`).
/// Panics with a descriptive message if any entry is missing a scheme,
/// so misconfiguration surfaces immediately at startup rather than silently
/// allowing unintended origins.
pub fn parse_origins(s: &str) -> Vec<HeaderValue> {
    s.split(',')
        .map(|o| o.trim())
        .filter(|o| !o.is_empty())
        .map(|o| {
            assert!(
                o.starts_with("http://") || o.starts_with("https://"),
                "CORS origin {o:?} is invalid: must start with http:// or https:// (without scheme)"
            );
            HeaderValue::from_str(o)
                .unwrap_or_else(|_| panic!("CORS origin {o:?} is not a valid header value"))
        })
        .collect()
}

/// Build a `CorsLayer` from the `ILINK_CORS_ORIGINS` environment variable.
///
/// - If the variable is unset or empty, returns a permissive layer (`*`).
/// - If set, returns a restrictive layer that only allows the listed origins.
pub fn build_cors_layer() -> CorsLayer {
    match std::env::var("ILINK_CORS_ORIGINS")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        None => CorsLayer::permissive(),
        Some(origins_str) => {
            let origins = parse_origins(&origins_str);
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
        }
    }
}

use crate::hub::HubState;
use pairing::*;
use routes::*;

/// Maximum simultaneous in-flight `sendmessage` requests across all clients.
/// Prevents a single burst of outbound messages from exhausting Hub worker threads.
const SENDMESSAGE_MAX_CONCURRENCY: usize = 64;

/// Middleware that logs every mutating admin API call with caller IP, method and path.
async fn admin_audit_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // Log before forwarding so the entry is written even if the handler panics.
    if matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        // `ConnectInfo` is only present when the router is served via
        // `into_make_service_with_connect_info`; in tests that use `oneshot()`
        // or plain `axum::serve`, the extension is absent and we log without the IP.
        let ip_str = req
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        if let Some(ip) = ip_str {
            tracing::info!(ip = %ip, %method, %path, "admin API call");
        } else {
            tracing::info!(%method, %path, "admin API call");
        }
    }
    next.run(req).await
}

pub fn build_router(state: Arc<HubState>) -> Router {
    // CORS is only required for the iLink-compatible bot API so that browser-based
    // SDK clients (e.g. OpenClaw) can call it from any origin.
    // Hub management and admin routes deliberately do NOT get CORS headers — they
    // should only be called server-side or via same-origin UI.
    let bot_cors = CorsLayer::permissive();

    let bot_api = Router::new()
        .route(
            "/ilink/bot/get_bot_qrcode",
            get(get_bot_qrcode).post(get_bot_qrcode_post),
        )
        .route("/ilink/bot/get_qrcode_status", get(get_qrcode_status))
        .route("/ilink/bot/getupdates", post(getupdates))
        .route(
            "/ilink/bot/sendmessage",
            post(sendmessage).layer(
                tower::ServiceBuilder::new()
                    .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
                    .layer(ConcurrencyLimitLayer::new(SENDMESSAGE_MAX_CONCURRENCY)),
            ),
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
        .route("/hub/clients/{name}/sessions", get(admin_client_sessions))
        .route(
            "/hub/clients/{name}/sessions/{session}/history",
            get(admin_client_session_history),
        )
        .route("/hub/ui", get(admin_ui))
        .route("/hub/pair/{code}", get(pair_page))
        .route("/hub/pair/{code}/confirm", post(pair_confirm))
        // iLink upstream management
        .route("/hub/ilink/status", get(admin_ilink_status))
        .route("/hub/ilink/relogin", post(admin_ilink_relogin))
        .route(
            "/hub/ilink/qr-stream-ticket",
            post(admin_ilink_qr_stream_ticket),
        )
        .route("/hub/ilink/qr-stream", get(admin_ilink_qr_stream))
        // Observability
        .route("/metrics", get(metrics))
        .route("/health", get(|| async { "ok" }))
        .layer(middleware::from_fn(admin_audit_log));

    let mcp_api = crate::mcp::mcp_router();

    Router::new()
        .merge(bot_api)
        .merge(admin_api)
        .merge(mcp_api)
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}
