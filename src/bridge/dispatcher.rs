use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::executor::{extract_media_env, run_cli, truncate_chars};
use super::AUTH_ERROR_KEYWORDS;
use crate::bridge::config::BridgeApp;
use crate::bridge::connection::hub_response_token_rejected;
use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, HubExt, SendMessageRequest,
    SendMessageResponse, WeixinMessage,
};

/// Returned from [`run_bridge`] when Hub terminates the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStop {
    /// Hub rejected the virtual token (401 / revoked).
    TokenRejected,
    /// CLI reported a fatal auth/credential error; user action required.
    FatalCliError(String),
    /// Graceful shutdown was requested (SIGTERM / Ctrl-C); bridge exited cleanly.
    Shutdown,
}

enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

/// Outcome of a single `HubClient::sendmessage` call. Lets callers distinguish
/// upstream throttling (ret == -2) from generic failures without inspecting
/// the raw HTTP body. HTTP transport errors and `ret` values other than 0
/// and -2 surface as `Err(_)` so the existing error-propagation paths are
/// preserved; the typed outcomes are only set when we got a parseable 2xx
/// response and want to communicate the semantic result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SendOutcome {
    /// Hub acknowledged the message.
    Sent,
    /// Hub signalled throttling / rate-limit (`ret == -2`). The call should
    /// be retried with backoff (M2/M3 will buffer + retry at the partial
    /// and final-reply layers). Carries the upstream errmsg for logging.
    ///
    /// Note: does NOT carry the unsent payload; callers must retain the
    /// original request if they wish to retry — M2 will restructure the
    /// partial-reply loop to do exactly that.
    Throttled { ret: i32, errmsg: Option<String> },
}

/// Sanitize an upstream `errmsg` string for safe logging.
///
/// Strips control characters (incl. CR/LF and ANSI escapes) and caps the
/// length so a maliciously long upstream message cannot pollute log lines or
/// buffer memory. Returns `None` when the input is `None` or empty after
/// sanitization.
fn sanitize_errmsg(s: Option<&str>) -> Option<String> {
    const MAX_LEN: usize = 256;
    let raw = s?;
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LEN)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Pure mapping from a parsed `SendMessageResponse` to a `SendOutcome`.
///
/// Extracted so the M1 typed semantics can be unit-tested without spinning
/// up an HTTP server. The (response_body_len, parse_ok) pair is carried
/// back so the caller can decide whether to surface an observability
/// warning — see [`parse_sendoutcome`].
#[allow(dead_code)]
fn classify_sendoutcome(parsed: Option<&SendMessageResponse>) -> SendOutcome {
    match parsed {
        None => SendOutcome::Sent,
        Some(v) => match v.ret {
            Some(0) => SendOutcome::Sent,
            Some(-2) => SendOutcome::Throttled {
                ret: -2,
                errmsg: v.errmsg.clone(),
            },
            Some(_other) => SendOutcome::Sent, // re-classified to Err by caller via ret != 0
            None => SendOutcome::Sent,
        },
    }
}

/// Map the raw HTTP response body of `sendmessage` into a [`SendOutcome`].
///
/// Empty bodies are treated as `Sent` (i.e. hub omitted its usual JSON
/// envelope). When the body parses as JSON and `ret` is some non-zero
/// value other than -2, this returns `Err` carrying the upstream ret/errmsg
/// so the caller can decide whether the value is fatal or transient. When
/// the body fails to parse entirely, this returns `Ok(Sent)` for backwards
/// compatibility with the legacy behaviour — but the caller is expected to
/// log a warning so the silent fallback is observable (see F-007).
fn parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SendOutcome::Sent);
    }
    match serde_json::from_str::<SendMessageResponse>(trimmed) {
        Ok(v) => {
            let ret = v.ret.unwrap_or(0);
            if ret == -2 {
                Ok(SendOutcome::Throttled {
                    ret: -2,
                    errmsg: v.errmsg,
                })
            } else if ret != 0 {
                Err((ret, v.errmsg))
            } else {
                Ok(SendOutcome::Sent)
            }
        }
        Err(_) => Ok(SendOutcome::Sent),
    }
}

#[derive(Clone)]
pub(super) struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    pub(super) fn new(hub_url: String, token: String) -> Self {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client");
        Self {
            http,
            hub_url,
            token,
        }
    }

    async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.clone(),
            base_info: Some(BaseInfo::default()),
            timeout: None,
        };
        let url = format!("{}/ilink/bot/getupdates", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let out: GetUpdatesResponse = resp.json().await?;
        if hub_response_token_rejected(status, out.ret) {
            warn!(
                status = %status,
                errmsg = ?out.errmsg,
                "hub rejected virtual token during getupdates"
            );
            return Ok(GetUpdatesOutcome::TokenRejected);
        }
        if !status.is_success() {
            anyhow::bail!("getupdates HTTP {status}: {:?}", out.errmsg);
        }
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        Ok(GetUpdatesOutcome::Ok(out))
    }

    async fn sendmessage(&self, req: SendMessageRequest) -> Result<SendOutcome> {
        let url = format!("{}/ilink/bot/sendmessage", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendmessage HTTP {status}: {t}");
        }
        let text = resp.text().await?;
        let body_len = text.len();
        match parse_sendoutcome(&text) {
            Ok(out) => {
                if body_len > 0 && matches!(out, SendOutcome::Sent) {
                    // We received a non-empty body that parsed either as
                    // ret==0 / None / failed JSON. Log a low-frequency
                    // observability line so silent fallback to Sent stays
                    // visible (F-007).
                    if serde_json::from_str::<SendMessageResponse>(&text).is_err() {
                        warn!(
                            body_len,
                            "sendmessage response body failed to parse as JSON; treating as Sent (legacy fallback)"
                        );
                    }
                }
                Ok(out)
            }
            Err((other, errmsg)) => {
                anyhow::bail!("sendmessage ret={other} errmsg={:?}", errmsg);
            }
        }
    }
}

fn session_dispatch_key(msg: &WeixinMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default");
    format!("{ctx}:{session_name}")
}

async fn run_session_worker(
    key: String,
    mut rx: mpsc::Receiver<WeixinMessage>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
) {
    const SESSION_WORKER_MAX_BACKOFF_SECS: u64 = 60;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Wait for next message; yield immediately if shutdown was already requested.
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            msg_opt = rx.recv() => match msg_opt {
                Some(m) => m,
                None => {
                    info!(session_key = %key, "session worker exiting");
                    return;
                }
            },
        };

        // Capture the routing identifiers needed for an error reply *before* moving `msg`
        // into `handle_one_message`, so that if the future is cancelled we can still send
        // feedback to the user.
        let ctx_for_err = msg.context_token.clone().unwrap_or_default();
        let from_for_err = msg.from_user_id.clone().unwrap_or_default();

        let result = tokio::select! {
            biased;
            // Shutdown arrived while we were processing — cancel the AI call and tell user.
            _ = shutdown.cancelled() => {
                if app.send_error_reply && !ctx_for_err.is_empty() {
                    let req = SendMessageRequest::reply(
                        ctx_for_err,
                        "⚠️ 响应中断（服务正在重启），请稍后重发消息".to_string(),
                        &from_for_err,
                    );
                    match client.sendmessage(req).await {
                        Ok(SendOutcome::Sent) => {}
                        Ok(SendOutcome::Throttled { ret, errmsg }) => {
                            // User did not receive the "service restarting,
                            // please resend" notice. This is an
                            // observability hit for the user. M3 must
                            // include this path when wiring buffer+retry.
                            warn!(
                                ret,
                                errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                                "sendmessage throttled during shutdown error reply; user did NOT receive restart notice — M3 must cover this path when adding buffer+retry"
                            );
                        }
                        Err(e) => warn!(error = %e, "failed to send shutdown error reply"),
                    }
                }
                return;
            }
            r = handle_one_message(&client, &app, msg, shutdown.clone()) => r,
        };

        match result {
            Ok(()) => {
                consecutive_failures = 0;
            }
            Err(HandleError::Fatal(reason)) => {
                error!(session_key = %key, reason = ?reason, "fatal CLI error; signalling bridge stop");
                let _ = stop_tx.send(Some(reason));
                return;
            }
            Err(HandleError::Transient(e)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff_secs =
                    SESSION_WORKER_MAX_BACKOFF_SECS.min(1_u64 << consecutive_failures.min(63));
                error!(
                    session_key = %key,
                    error = %e,
                    consecutive_failures,
                    backoff_secs,
                    "message handler failed; backing off before next message"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
        }
    }
}

enum HandleError {
    Transient(anyhow::Error),
    Fatal(BridgeStop),
}

impl From<anyhow::Error> for HandleError {
    fn from(e: anyhow::Error) -> Self {
        HandleError::Transient(e)
    }
}

const DEFAULT_SESSION_QUEUE_SIZE: usize = 200;
/// Maximum number of concurrent session workers. Each worker holds an open mpsc channel and a
/// spawned Tokio task; unbounded growth would exhaust both. When the cap is reached, the oldest
/// idle (closed-channel) entries are evicted first; if all entries are still active the new
/// message is dropped with a warning.
const MAX_SESSION_WORKERS: usize = 512;

struct SessionDispatcher {
    // std::sync::Mutex is correct here: the critical section contains only synchronous
    // HashMap operations (retain/get/insert) with no await points.
    senders: std::sync::Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
}

impl SessionDispatcher {
    fn new(
        client: HubClient,
        app: Arc<BridgeApp>,
        stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            senders: std::sync::Mutex::new(HashMap::new()),
            client,
            app,
            stop_tx,
            shutdown,
        }
    }

    async fn dispatch(&self, msg: WeixinMessage) {
        let key = session_dispatch_key(&msg);
        let mut senders = self
            .senders
            .lock()
            .expect("SessionDispatcher senders lock poisoned");

        // Check if a live worker already exists for this key.
        let needs_new = match senders.get(&key) {
            Some(tx) => tx.is_closed(),
            None => true,
        };

        if needs_new {
            // Evict closed entries only when we need to make room — avoids O(N)
            // retain on every message. The background evict_closed_senders task
            // handles periodic cleanup so the map doesn't grow unbounded.
            if senders.len() >= MAX_SESSION_WORKERS {
                senders.retain(|_, tx| !tx.is_closed());
                if senders.len() >= MAX_SESSION_WORKERS {
                    warn!(
                        session_key = %key,
                        cap = MAX_SESSION_WORKERS,
                        active = senders.len(),
                        "session worker cap reached, dropping message"
                    );
                    return;
                }
            }
            let (tx, rx) = mpsc::channel(DEFAULT_SESSION_QUEUE_SIZE);
            senders.insert(key.clone(), tx.clone());
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            let stop_tx = self.stop_tx.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(run_session_worker(
                key.clone(),
                rx,
                client,
                app,
                stop_tx,
                shutdown,
            ));
        }

        if let Some(tx) = senders.get(&key) {
            match tx.try_send(msg) {
                Ok(_) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(session_key = %key, "session queue full, dropping message");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    /// Remove closed sender entries. Called periodically by a background task
    /// so the map doesn't accumulate dead entries between cap-enforcement evictions.
    fn evict_closed_senders(&self) {
        if let Ok(mut senders) = self.senders.lock() {
            senders.retain(|_, tx| !tx.is_closed());
        }
    }

    #[cfg(test)]
    fn sender_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .senders
            .lock()
            .expect("senders poisoned")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns when Hub signals a stop condition (token rejected or fatal CLI auth error).
/// Pass a [`CancellationToken`] to request graceful shutdown: in-flight AI calls are
/// cancelled and the user receives an error notification before the function returns.
pub async fn run_bridge_with_shutdown(
    hub_url: String,
    token: String,
    app: BridgeApp,
    shutdown: CancellationToken,
) -> BridgeStop {
    let client = HubClient::new(hub_url, token);
    let app = Arc::new(app);
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(None::<BridgeStop>);
    let dispatcher = Arc::new(SessionDispatcher::new(
        client.clone(),
        Arc::clone(&app),
        stop_tx,
        shutdown.clone(),
    ));
    let mut buf = String::new();
    let mut backoff_secs: u64 = 3;
    const MAX_BACKOFF_SECS: u64 = 60;

    // Periodically evict closed sender entries so the senders map doesn't
    // accumulate dead entries between cap-enforcement evictions on the hot path.
    {
        let dispatcher_weak = Arc::downgrade(&dispatcher);
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_clone.cancelled() => return,
                    _ = interval.tick() => {
                        if let Some(d) = dispatcher_weak.upgrade() {
                            d.evict_closed_senders();
                        } else {
                            return;
                        }
                    }
                }
            }
        });
    }

    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        // Check if any session worker signalled a fatal stop.
        if stop_rx.has_changed().unwrap_or(false) {
            if let Some(reason) = stop_rx.borrow_and_update().clone() {
                return reason;
            }
        }

        let getupdates_fut = client.getupdates(&mut buf);
        let resp = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                // Give in-flight session workers a moment to send error replies before exit.
                tokio::time::sleep(Duration::from_secs(2)).await;
                return BridgeStop::Shutdown;
            }
            r = getupdates_fut => match r {
                Ok(GetUpdatesOutcome::Ok(r)) => {
                    backoff_secs = 3;
                    r
                }
                Ok(GetUpdatesOutcome::TokenRejected) => return BridgeStop::TokenRejected,
                Err(e) => {
                    error!(error = %e, backoff_secs, "getupdates failed; retrying with backoff");
                    let sleep = tokio::time::sleep(Duration::from_secs(backoff_secs));
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return BridgeStop::Shutdown;
                        }
                        _ = sleep => {}
                    }
                    continue;
                }
            },
        };

        if resp.ret != Some(0) {
            warn!(
                ret = ?resp.ret,
                errcode = ?resp.errcode,
                errmsg = ?resp.errmsg,
                "getupdates returned non-zero ret"
            );
        }

        for msg in resp.msgs.unwrap_or_default() {
            dispatcher.dispatch(msg).await;
        }
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns when Hub signals a stop condition (token rejected or fatal CLI auth error).
/// For graceful shutdown support (SIGTERM / Ctrl-C), use [`run_bridge_with_shutdown`].
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) -> BridgeStop {
    run_bridge_with_shutdown(hub_url, token, app, CancellationToken::new()).await
}

fn dump_inbound_weixin_message_for_debug(msg: &WeixinMessage) {
    let Ok(flag) = std::env::var("ILINKHUB_BRIDGE_DUMP_MSG") else {
        return;
    };
    let f = flag.trim().to_ascii_lowercase();
    if !matches!(f.as_str(), "1" | "true" | "yes") {
        return;
    }

    let full = serde_json::to_string_pretty(msg)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialize WeixinMessage: {e}\"}}"));
    eprintln!("========== ILINKHUB_BRIDGE_DUMP_MSG: full WeixinMessage (JSON) ==========");
    eprintln!("{full}");
    eprintln!("========== end full message ==========");

    if let Some(items) = msg.item_list.as_ref() {
        for (i, item) in items.iter().enumerate() {
            let extra = serde_json::to_string_pretty(&item.extra)
                .unwrap_or_else(|_| "\"<extra serialize error>\"".to_string());
            eprintln!("---------- item_list[{i}] ----------");
            eprintln!("  type (item_type): {:?}", item.item_type);
            eprintln!("  text_item: {:?}", item.text_item);
            eprintln!("  extra (flattened fields from iLink, not in text_item):");
            eprintln!("{extra}");
        }
        eprintln!("========== end item_list dump ==========");
    } else {
        eprintln!("========== item_list: <none> ==========");
    }
}

#[tracing::instrument(
    skip_all,
    fields(
        from    = msg.from_user_id.as_deref().unwrap_or("?"),
        ctx     = msg.context_token.as_deref().unwrap_or("(none)"),
        profile = tracing::field::Empty,
    )
)]
async fn handle_one_message(
    client: &HubClient,
    app: &BridgeApp,
    msg: WeixinMessage,
    shutdown: CancellationToken,
) -> Result<(), HandleError> {
    dump_inbound_weixin_message_for_debug(&msg);

    if app.skip_bot_messages && msg.message_type == Some(2) {
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.to_string(),
        None if !app.require_text => String::new(),
        None => return Ok(()),
    };
    if text.trim().is_empty() && app.require_text {
        return Ok(());
    }

    let media_env = extract_media_env(&msg);

    let (profile_name, profile, payload) = app
        .resolve(&text)
        .with_context(|| format!("route message for profile (text prefix): {text:?}"))?;

    let ctx = msg
        .context_token
        .clone()
        .filter(|s| !s.is_empty())
        .context("inbound message missing context_token")?;
    let from_user = msg.from_user_id.clone().unwrap_or_default();
    let session_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.as_deref())
        .unwrap_or("")
        .to_string();
    let session_name_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default")
        .to_string();

    tracing::Span::current().record("profile", profile_name);
    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, "running bridge profile");

    let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<String>();

    let fwd_client = client.clone();
    let fwd_ctx = ctx.clone();
    let fwd_from_user = from_user.clone();
    let fwd_session_name = session_name_for_cli.clone();
    let fwd_shutdown = shutdown.clone();
    let forward_handle = tokio::spawn(async move {
        loop {
            // Cancel-safety: even if a chunk is currently being sent
            // (up to 90s for the hub round-trip per HubClient::new), we
            // bail as soon as shutdown is signalled so the run_session_worker
            // can return without dragging the supervisor wait window out.
            let chunk = tokio::select! {
                biased;
                _ = fwd_shutdown.cancelled() => return,
                chunk_opt = partial_rx.recv() => match chunk_opt {
                    Some(c) => c,
                    None => return,
                },
            };
            let mut req =
                SendMessageRequest::reply_text(fwd_ctx.clone(), chunk, &fwd_from_user, None);
            if let Some(ref mut msg) = req.msg {
                let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                ext.session_name = Some(fwd_session_name.clone());
            }
            // Wrap the actual send in a select too so an in-flight
            // sendmessage can be aborted by shutdown.
            let send_fut = fwd_client.sendmessage(req);
            let send_result = tokio::select! {
                biased;
                _ = fwd_shutdown.cancelled() => return,
                r = send_fut => r,
            };
            match send_result {
                Ok(SendOutcome::Sent) => {}
                Ok(SendOutcome::Throttled { ret, errmsg }) => {
                    // M1 placeholder: at this site the chunk is already gone
                    // from the unbounded channel, so we cannot re-enqueue it
                    // without restructuring the loop. Log loudly so the M2
                    // change can verify its coverage by watching for this
                    // message in test runs.
                    warn!(
                        ret,
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "sendmessage throttled for partial reply; M2 will buffer+retry here"
                    );
                }
                Err(e) => warn!(error = %e, "failed to send partial reply"),
            }
        }
    });

    let cli_result = run_cli(
        profile,
        profile_name,
        &payload,
        &session_for_cli,
        &session_name_for_cli,
        &from_user,
        &ctx,
        &media_env,
        partial_tx,
    )
    .await;

    let _ = forward_handle.await;

    match cli_result {
        Ok((raw_body, cli_session)) => {
            let body = truncate_chars(
                &raw_body,
                profile.max_reply_chars,
                &profile.truncation_suffix,
            );
            if body.trim().is_empty() {
                if let Some(sid) = cli_session {
                    if !sid.trim().is_empty() {
                        let mut req = SendMessageRequest::reply_text(
                            ctx,
                            String::new(),
                            &from_user,
                            Some(sid),
                        );
                        if let Some(ref mut msg) = req.msg {
                            let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                            hub_ext.session_name = Some(session_name_for_cli.clone());
                        }
                        match client.sendmessage(req).await {
                            Ok(SendOutcome::Sent) => {}
                            Ok(SendOutcome::Throttled { ret, errmsg }) => {
                                warn!(
                                    ret,
                                    errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                                    "sendmessage throttled while persisting cli_session_id; M3 will buffer+retry here"
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to persist cli_session_id after ILINK_PARTIAL-only reply")
                            }
                        }
                    }
                }
                return Ok(());
            }
            let mut req = SendMessageRequest::reply_text(ctx, body, &from_user, cli_session);
            if let Some(ref mut msg) = req.msg {
                let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                hub_ext.session_name = Some(session_name_for_cli.clone());
            }
            match client.sendmessage(req).await {
                Ok(SendOutcome::Sent) => {}
                Ok(SendOutcome::Throttled { ret, errmsg }) => {
                    // M1 placeholder: final-reply path does NOT escalate
                    // Throttled to HandleError. M3 will buffer + retry the
                    // final reply on throttling and remove this warn
                    // entirely (or convert it into a structured "retry
                    // attempted" trace). For now we surface the typed
                    // signal so M3 coverage can be verified by watching
                    // for this log line.
                    warn!(
                        ret,
                        errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                        "sendmessage throttled on final reply; M3 will buffer+retry here"
                    );
                }
                Err(e) => return Err(HandleError::from(e.context("sendmessage reply"))),
            }
        }
        Err(e) => {
            if app.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let req = SendMessageRequest::reply(ctx, err_text, &from_user);
                match client.sendmessage(req).await {
                    Ok(SendOutcome::Sent) => {}
                    Ok(SendOutcome::Throttled { ret, errmsg }) => {
                        warn!(
                            ret,
                            errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                            "sendmessage throttled on CLI-error reply; M3 will buffer+retry here"
                        );
                    }
                    Err(send_e) => warn!(error = %send_e, "failed to send error reply"),
                }
            }
            let err_str = e.to_string().to_lowercase();
            if AUTH_ERROR_KEYWORDS.iter().any(|&k| err_str.contains(k))
                || err_str.contains("not found")
                || err_str.contains("no such file")
            {
                return Err(HandleError::Fatal(BridgeStop::FatalCliError(e.to_string())));
            }
            return Err(HandleError::Transient(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem};

    fn make_msg(ctx: &str, session_name: &str) -> WeixinMessage {
        WeixinMessage {
            context_token: Some(ctx.into()),
            ilink_hub_ext: Some(HubExt {
                session_id: Some(String::new()),
                session_name: Some(session_name.into()),
                cli_session_id: None,
            }),
            item_list: Some(std::sync::Arc::new(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                ..Default::default()
            }])),
            from_user_id: Some("user1".into()),
            ..Default::default()
        }
    }

    fn make_fast_app() -> BridgeApp {
        BridgeApp::parse_yaml(
            r#"
command: echo
args: []
stdin: none
timeout_secs: 5
"#,
        )
        .unwrap()
    }

    fn fake_client() -> HubClient {
        HubClient::new("http://127.0.0.1:1".into(), "test-token".into())
    }

    fn make_stop_tx() -> tokio::sync::watch::Sender<Option<BridgeStop>> {
        tokio::sync::watch::channel(None).0
    }

    #[test]
    fn key_combines_ctx_and_session_name() {
        assert_eq!(
            session_dispatch_key(&make_msg("ctx-123", "feat-a")),
            "ctx-123:feat-a"
        );
    }

    #[test]
    fn key_defaults_session_name_when_ext_absent() {
        let msg = WeixinMessage {
            context_token: Some("ctx-x".into()),
            ilink_hub_ext: None,
            ..Default::default()
        };
        assert_eq!(session_dispatch_key(&msg), "ctx-x:default");
    }

    #[test]
    fn key_uses_empty_string_when_ctx_absent() {
        let msg = WeixinMessage {
            context_token: None,
            ilink_hub_ext: None,
            ..Default::default()
        };
        assert_eq!(session_dispatch_key(&msg), ":default");
    }

    #[test]
    fn key_differs_for_different_session_names() {
        let a = make_msg("ctx", "session-a");
        let b = make_msg("ctx", "session-b");
        assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
    }

    #[test]
    fn key_differs_for_different_ctx_tokens() {
        let a = make_msg("ctx-1", "default");
        let b = make_msg("ctx-2", "default");
        assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
    }

    #[tokio::test]
    async fn same_key_reuses_single_sender() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-a", "default");
        disp.dispatch(msg.clone()).await;
        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-a:default"]);
    }

    #[tokio::test]
    async fn different_ctx_tokens_get_separate_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-a", "default")).await;
        disp.dispatch(make_msg("ctx-b", "default")).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-a:default", "ctx-b:default"]);
    }

    #[tokio::test]
    async fn different_session_names_get_separate_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-a", "feature-x")).await;
        disp.dispatch(make_msg("ctx-a", "feature-y")).await;
        assert_eq!(
            disp.sender_keys(),
            vec!["ctx-a:feature-x", "ctx-a:feature-y"]
        );
    }

    #[tokio::test]
    async fn three_distinct_sessions_create_three_senders() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        disp.dispatch(make_msg("ctx-1", "default")).await;
        disp.dispatch(make_msg("ctx-2", "default")).await;
        disp.dispatch(make_msg("ctx-1", "feature-a")).await;
        assert_eq!(
            disp.sender_keys(),
            vec!["ctx-1:default", "ctx-1:feature-a", "ctx-2:default"]
        );
    }

    #[tokio::test]
    async fn repeated_same_key_does_not_grow_sender_map() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-x", "s1");
        for _ in 0..5 {
            disp.dispatch(msg.clone()).await;
        }
        assert_eq!(disp.sender_keys().len(), 1);
    }

    #[tokio::test]
    async fn dead_sender_triggers_new_worker_on_next_dispatch() {
        let disp = SessionDispatcher::new(
            fake_client(),
            Arc::new(make_fast_app()),
            make_stop_tx(),
            CancellationToken::new(),
        );
        let msg = make_msg("ctx-z", "default");

        disp.dispatch(msg.clone()).await;

        {
            let mut senders = disp.senders.lock().expect("senders poisoned");
            senders.remove("ctx-z:default");
        }
        assert_eq!(disp.sender_keys().len(), 0);

        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys(), vec!["ctx-z:default"]);
    }

    // ─── parse_sendoutcome ─────────────────────────────────────────────
    //
    // M1 review F-009: pin the body-text → SendOutcome mapping with unit
    // tests so M2/M3 retry decisions have a stable oracle. The legacy
    // behaviour (parse failure → Sent) is intentionally preserved; tests
    // make that explicit so future changes are a deliberate decision.

    #[test]
    fn parse_empty_body_is_sent() {
        assert_eq!(parse_sendoutcome(""), Ok(SendOutcome::Sent));
        assert_eq!(parse_sendoutcome("   \n\t "), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_zero_is_sent() {
        let body = r#"{"ret":0}"#;
        assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_none_is_sent() {
        let body = r#"{}"#;
        assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
    }

    #[test]
    fn parse_ret_negative_two_is_throttled() {
        let body = r#"{"ret":-2,"errmsg":"rate limited"}"#;
        match parse_sendoutcome(body).unwrap() {
            SendOutcome::Throttled { ret, errmsg } => {
                assert_eq!(ret, -2);
                assert_eq!(errmsg.as_deref(), Some("rate limited"));
            }
            other => panic!("expected Throttled, got {:?}", other),
        }
    }

    #[test]
    fn parse_ret_other_non_zero_is_err() {
        let body = r#"{"ret":1,"errmsg":"oops"}"#;
        match parse_sendoutcome(body) {
            Err((1, Some(m))) => assert_eq!(m, "oops"),
            other => panic!("expected Err((1, Some(..))), got {:?}", other),
        }
        let body = r#"{"ret":-99,"errmsg":"unknown"}"#;
        match parse_sendoutcome(body) {
            Err((-99, Some(m))) => assert_eq!(m, "unknown"),
            other => panic!("expected Err((-99, Some(..))), got {:?}", other),
        }
    }

    #[test]
    fn parse_unparseable_body_falls_back_to_sent() {
        // Legacy behaviour: pre-M1 code returned Ok(Sent) on JSON parse
        // failure. M1 keeps the fallback but the dispatcher now emits a
        // warn log; the unit test pins the fallback itself.
        assert_eq!(parse_sendoutcome("not json"), Ok(SendOutcome::Sent));
        assert_eq!(parse_sendoutcome(r#"{"ret": "#), Ok(SendOutcome::Sent));
    }

    // ─── Adversarial / property-style tests ────────────────────────────
    //
    // Goal: catch silent regressions in the parser that could let a
    // hostile hub payload (e.g. ret==-2 disguised as Sent) bypass M2/M3
    // retry logic.

    #[test]
    fn adversarial_ret_negative_two_never_becomes_sent() {
        // Even with errmsg present, ret==-2 must produce Throttled —
        // this is the entire reason SendOutcome exists.
        for body in [
            r#"{"ret":-2}"#,
            r#"{"ret":-2,"errmsg":""}"#,
            r#"{"ret":-2,"errmsg":"hi"}"#,
            r#" {"ret":-2} "#,
            r#"{"ret":-2,"errmsg":"a]lo t of\\n junk"}"#,
        ] {
            match parse_sendoutcome(body).unwrap() {
                SendOutcome::Throttled { ret: -2, .. } => {}
                other => panic!("ret=-2 body {:?} misclassified as {:?}", body, other),
            }
        }
    }

    #[test]
    fn adversarial_large_payload_does_not_panic() {
        // Hub returning a multi-megabyte body must not panic / OOM the
        // parser. We send 4 MB of repeated JSON-safe content and confirm
        // we get a typed outcome (either Sent, Throttled, or parse-fail)
        // rather than a process abort.
        let big = "x".repeat(4 * 1024 * 1024);
        let body = format!(r#"{{"ret":0,"errmsg":"{}"}}"#, big);
        let res = parse_sendoutcome(&body);
        // We don't care which branch it took — only that it didn't panic.
        let _ = res;
    }

    #[test]
    fn adversarial_nested_garbage_does_not_panic() {
        // Hub could send deeply nested or weird JSON. As long as serde
        // rejects it, parse_sendoutcome must fall back to Sent and not
        // propagate a panic.
        for body in [
            r#"{"ret":{"deeply":{"nested":[1,2,3]}}}"#,
            r#"{"ret":-2.5}"#,
            r#"{"ret":-2,"errmsg":12345}"#,
            r#"{} broken"#,
        ] {
            let _ = parse_sendoutcome(body);
        }
    }

    // ─── sanitize_errmsg ───────────────────────────────────────────────

    #[test]
    fn sanitize_strips_control_chars() {
        let dirty = "before\r\nafter\tend\x1b[31mred\x1b[0m";
        let cleaned = sanitize_errmsg(Some(dirty)).unwrap();
        assert!(!cleaned.contains('\r'));
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\t'));
        assert!(!cleaned.contains('\x1b'));
        assert!(cleaned.contains("before"));
        assert!(cleaned.contains("red"));
    }

    #[test]
    fn sanitize_caps_length() {
        let huge = "a".repeat(10_000);
        let cleaned = sanitize_errmsg(Some(&huge)).unwrap();
        assert_eq!(cleaned.len(), 256);
    }

    #[test]
    fn sanitize_handles_none_and_empty() {
        assert!(sanitize_errmsg(None).is_none());
        assert!(sanitize_errmsg(Some("")).is_none());
        assert!(sanitize_errmsg(Some("\r\n\t")).is_none());
    }

    #[test]
    fn sanitize_preserves_printable_unicode() {
        // Multi-byte UTF-8 and punctuation must survive.
        let s = "你好, world! 🌏 — dash";
        assert_eq!(sanitize_errmsg(Some(s)).as_deref(), Some(s));
    }

    // ─── classify_sendoutcome (additional M1F-006 helper) ──────────────

    #[test]
    fn classify_three_categories() {
        let none_resp = SendMessageResponse {
            ret: None,
            errmsg: None,
        };
        let zero_resp = SendMessageResponse {
            ret: Some(0),
            errmsg: None,
        };
        let tmo_resp = SendMessageResponse {
            ret: Some(-2),
            errmsg: Some("rl".into()),
        };
        assert_eq!(classify_sendoutcome(None), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&none_resp)), SendOutcome::Sent);
        assert_eq!(classify_sendoutcome(Some(&zero_resp)), SendOutcome::Sent);
        assert_eq!(
            classify_sendoutcome(Some(&tmo_resp)),
            SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into())
            }
        );
    }

    #[test]
    fn outcome_clone_works() {
        // Pin F-006's derive(Clone) at compile-time — if Clone goes away
        // this won't compile.
        let original = SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("x".into()),
        };
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }
}
