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
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::hub::HubState;
use pairing::*;
use routes::*;

fn parse_origins(raw: &str) -> Vec<HeaderValue> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
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
            CorsLayer::new().allow_origin(AllowOrigin::list(origins))
        }
        _ => CorsLayer::permissive(),
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
    #[should_panic(expected = "invalid origin")]
    fn origins_rejects_control_chars() {
        parse_origins("bad\norigin");
    }
}
