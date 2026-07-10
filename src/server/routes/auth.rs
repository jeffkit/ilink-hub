//! Auth helpers for bot vtoken and admin Bearer token.
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, StatusCode},
    Json,
};
use std::sync::Arc;

use crate::hub::HubState;

pub const UNKNOWN_VTOKEN_MSG: &str = "Unknown or revoked virtual token; register via POST /hub/register or ilink-hub-bridge --force-register";

// ─── Auth helpers ─────────────────────────────────────────────────────────────

/// Schema check for Hub-issued virtual tokens. Tokens are minted in
/// `hub::registry::ClientInfo::new` as `vhub_{uuid v4 simple}` (32 lowercase
/// hex chars). Reject anything that does not match before doing registry work,
/// so a misconfigured client cannot inject iLink-style bot tokens
/// (`botid@im.bot:secret`) into the vtoken lookup path.
pub(crate) fn is_valid_vtoken(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("vhub_") else {
        return false;
    };
    rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Public re-export for the MCP router.
pub fn extract_vtoken_pub(headers: &axum::http::HeaderMap) -> Option<String> {
    extract_vtoken(headers)
}

pub(super) fn extract_vtoken(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| is_valid_vtoken(s))
        .map(crate::hub::hash_vtoken)
}

pub(super) fn check_admin_auth(admin: &crate::hub::AdminConfig, headers: &HeaderMap) -> bool {
    if let Some(required) = &admin.token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        use subtle::ConstantTimeEq;
        return provided.as_bytes().ct_eq(required.as_bytes()).unwrap_u8() == 1;
    }
    admin.insecure_no_auth
}

/// Axum extractor that enforces admin authentication. Any route that extracts
/// `AdminGuard` is automatically protected — no per-handler `check_admin_auth`
/// call needed. New admin routes added in the future cannot forget auth.
pub struct AdminGuard;

impl FromRequestParts<Arc<HubState>> for AdminGuard {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<HubState>,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        if check_admin_auth(&state.admin, headers) {
            Ok(AdminGuard)
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            ))
        }
    }
}
