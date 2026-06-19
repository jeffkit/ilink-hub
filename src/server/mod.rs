pub mod pairing;
pub mod routes;
pub mod sse_ticket;

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Request},
    middleware::{self, Next},
    response::Response,
    routing::{get, patch, post},
    Router,
};
use routes::{
    admin_ilink_qr_stream, admin_ilink_qr_stream_ticket, admin_ilink_relogin, admin_ilink_status,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::hub::HubState;
use pairing::*;
use routes::*;

/// Maximum simultaneous in-flight `sendmessage` requests across all clients.
/// Prevents a single burst of outbound messages from exhausting Hub worker threads.
const SENDMESSAGE_MAX_CONCURRENCY: usize = 64;

/// Middleware that logs every mutating admin API call with caller IP, method and path.
async fn admin_audit_log(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // Log before forwarding so the entry is written even if the handler panics.
    if matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        tracing::info!(
            ip = %addr.ip(),
            %method,
            %path,
            "admin API call"
        );
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

    Router::new()
        .merge(bot_api)
        .merge(admin_api)
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}
