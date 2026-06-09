pub mod pairing;
pub mod routes;

use axum::{
    routing::{get, patch, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::hub::HubState;
use pairing::*;
use routes::*;

pub fn build_router(state: Arc<HubState>) -> Router {
    Router::new()
        // iLink-compatible API (same paths as ilinkai.weixin.qq.com)
        .route(
            "/ilink/bot/get_bot_qrcode",
            get(get_bot_qrcode).post(get_bot_qrcode_post),
        )
        .route("/ilink/bot/get_qrcode_status", get(get_qrcode_status))
        .route("/ilink/bot/getupdates", post(getupdates))
        .route("/ilink/bot/sendmessage", post(sendmessage))
        .route("/ilink/bot/sendtyping", post(sendtyping))
        .route("/ilink/bot/getconfig", post(getconfig))
        .route("/ilink/bot/getuploadurl", post(getuploadurl))
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
        // Observability
        .route("/metrics", get(metrics))
        .route("/health", get(|| async { "ok" }))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
