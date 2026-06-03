pub mod routes;

use std::sync::Arc;
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::hub::HubState;
use routes::*;

pub fn build_router(state: Arc<HubState>) -> Router {
    Router::new()
        // iLink-compatible API (same paths as ilinkai.weixin.qq.com)
        .route("/ilink/bot/getupdates", post(getupdates))
        .route("/ilink/bot/sendmessage", post(sendmessage))
        .route("/ilink/bot/sendtyping", post(sendtyping))
        .route("/ilink/bot/getconfig", post(getconfig))
        .route("/ilink/bot/getuploadurl", post(getuploadurl))
        // Hub management (non-iLink)
        .route("/hub/register", post(register))
        .route("/hub/clients", get(admin_clients))
        .route("/health", get(|| async { "ok" }))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
