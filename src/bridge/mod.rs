//! CLI bridge: connect to iLink Hub as a virtual-token backend and run a local command per text message.
//! Supports **single-profile YAML** (flat `command` / `args`) or **multi-profile YAML**
//! (`profiles` + `routing`: `fixed` or `prefix`).
//!
//! Used by the `ilink-hub-bridge` binary; see `docs/bridge/README.md`.

pub mod builtin;
mod config;
mod connection;
pub mod manager;

pub use config::{BridgeApp, BridgeConfig, BridgeProfile, RoutingStrategy, StdinMode};
pub use connection::{
    default_auto_client_name, default_local_credential_path, hub_response_token_rejected,
    resolve_hub_connection, validate_hub_token,
};

/// Returned from [`run_bridge`] when Hub rejects the virtual token at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStop {
    TokenRejected,
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, SendMessageRequest, SendMessageResponse,
    WeixinMessage,
};
use crate::paths::expand_user_path;

/// Keywords in CLI stderr that indicate an auth/credential problem (missing login, expired token, etc.).
/// When any of these appear in the error output, the bridge treats the failure as fatal (needs user action).
const AUTH_ERROR_KEYWORDS: &[&str] = &[
    "login",
    "logout",
    "auth",
    "credential",
    "sign in",
    "unauthorized",
    "unauthenticated",
    "401",
    "not logged in",
    "keychain",
    "api key",
    "token",
];

enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

#[derive(Clone)]
struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    fn new(hub_url: String, token: String) -> Self {
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

    async fn sendmessage(&self, req: SendMessageRequest) -> Result<()> {
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
        if text.trim().is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<SendMessageResponse>(&text) {
            Ok(v) => {
                if v.ret.map(|r| r != 0).unwrap_or(false) {
                    anyhow::bail!("sendmessage ret={:?} errmsg={:?}", v.ret, v.errmsg);
                }
                Ok(())
            }
            Err(_) => Ok(()),
        }
    }
}

// ─── Session-level parallel dispatch ─────────────────────────────────────────

/// Computes the dispatch key that determines serialization boundaries.
///
/// Messages with the **same key** are processed serially by one worker task.
/// Messages with **different keys** may run concurrently in separate Tokio tasks.
///
/// Key = `"{context_token}:{session_name}"`:
/// - `context_token` identifies the WeChat conversation (one per DM / group).
/// - `session_name`  is the named Claude session inside that conversation (default: `"default"`).
///
/// | Scenario                          | Same key? | Behaviour |
/// |-----------------------------------|-----------|-----------|
/// | Same user, same session           | ✓         | Serial    |
/// | Same user, different sessions     | ✗         | Parallel  |
/// | Different users / group chats     | ✗         | Parallel  |
fn session_dispatch_key(msg: &WeixinMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default");
    format!("{ctx}:{session_name}")
}

/// Per-session serial worker — runs until the sender side of its channel is dropped.
///
/// Processes messages one at a time so that `--resume` calls for the same Claude session
/// never race against each other.
///
/// On repeated CLI failures the worker applies exponential backoff (up to
/// [`SESSION_WORKER_MAX_BACKOFF_SECS`]) before processing the next message, preventing
/// tight crash-loops when the underlying CLI binary is unavailable or misconfigured.
async fn run_session_worker(
    key: String,
    mut rx: mpsc::Receiver<WeixinMessage>,
    client: HubClient,
    app: Arc<BridgeApp>,
) {
    const SESSION_WORKER_MAX_BACKOFF_SECS: u64 = 60;
    let mut consecutive_failures: u32 = 0;

    while let Some(msg) = rx.recv().await {
        match handle_one_message(&client, &app, msg).await {
            Ok(()) => {
                consecutive_failures = 0;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                // Exponential backoff: 1s, 2s, 4s, … up to SESSION_WORKER_MAX_BACKOFF_SECS.
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
    info!(session_key = %key, "session worker exiting");
}

/// Routes inbound messages to per-session serial workers.
///
/// Each unique dispatch key owns a bounded `Sender`; a new Tokio task is spawned lazily
/// on first use. If a worker task exits unexpectedly (channel closed), the next `dispatch`
/// call for that key transparently creates a fresh worker. Messages are dropped with a
/// warning when the per-session queue is full (backpressure).
const DEFAULT_SESSION_QUEUE_SIZE: usize = 200;
struct SessionDispatcher {
    senders: tokio::sync::Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
    client: HubClient,
    app: Arc<BridgeApp>,
}

impl SessionDispatcher {
    fn new(client: HubClient, app: Arc<BridgeApp>) -> Self {
        Self {
            senders: tokio::sync::Mutex::new(HashMap::new()),
            client,
            app,
        }
    }

    /// Route `msg` to the correct session worker, spawning one if necessary.
    async fn dispatch(&self, msg: WeixinMessage) {
        let key = session_dispatch_key(&msg);
        let mut senders = self.senders.lock().await;

        // Opportunistically evict dead senders to prevent unbounded map growth.
        // Workers exit when their receiver is dropped; is_closed() detects this without
        // scanning all entries on every dispatch — just drop whatever is already dead.
        senders.retain(|_, tx| !tx.is_closed());

        // Try the existing sender; if the channel is dead, fall through to spawn a new worker.
        let needs_new = match senders.get(&key) {
            Some(tx) => tx.is_closed(),
            None => true,
        };

        if needs_new {
            let (tx, rx) = mpsc::channel(DEFAULT_SESSION_QUEUE_SIZE);
            senders.insert(key.clone(), tx.clone());
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            tokio::spawn(run_session_worker(key.clone(), rx, client, app));
        }

        // Send to the (possibly freshly created) worker; drop on backpressure.
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

    /// Returns sorted dispatch keys currently in the sender map. Used by tests only.
    #[cfg(test)]
    async fn sender_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.senders.lock().await.keys().cloned().collect();
        keys.sort();
        keys
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns [`BridgeStop::TokenRejected`] when Hub returns 401 for an unknown/revoked vtoken.
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) -> BridgeStop {
    let client = HubClient::new(hub_url, token);
    let app = Arc::new(app);
    let dispatcher = SessionDispatcher::new(client.clone(), Arc::clone(&app));
    let mut buf = String::new();
    // Exponential backoff for getupdates errors (Hub down / transient network).
    // Caps at 60s; resets to initial value on the first successful poll.
    let mut backoff_secs: u64 = 3;
    const MAX_BACKOFF_SECS: u64 = 60;

    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        let resp = match client.getupdates(&mut buf).await {
            Ok(GetUpdatesOutcome::Ok(r)) => {
                backoff_secs = 3; // reset on success
                r
            }
            Ok(GetUpdatesOutcome::TokenRejected) => return BridgeStop::TokenRejected,
            Err(e) => {
                error!(error = %e, backoff_secs, "getupdates failed; retrying with backoff");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                continue;
            }
        };

        if resp.ret != Some(0) {
            warn!(
                ret = ?resp.ret,
                errcode = ?resp.errcode,
                errmsg = ?resp.errmsg,
                "getupdates returned non-zero ret"
            );
        }

        // Each message is dispatched to its session worker without blocking the poll loop.
        // Messages for different sessions execute concurrently; same-session messages are
        // serialised inside the worker's channel queue.
        for msg in resp.msgs.unwrap_or_default() {
            dispatcher.dispatch(msg).await;
        }
    }
}

/// When `ILINKHUB_BRIDGE_DUMP_MSG` is `1` / `true` / `yes`, print the inbound message to stderr.
///
/// Shows the JSON shape **after Hub → serde** (same struct downstream always sees). Top-level
/// fields unknown to [`WeixinMessage`](crate::ilink::types::WeixinMessage) are already dropped at
/// deserialize; nested keys merged into each [`MessageItem`](crate::ilink::types::MessageItem)'s
/// `extra` are visible here.
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
async fn handle_one_message(client: &HubClient, app: &BridgeApp, msg: WeixinMessage) -> Result<()> {
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

    // Channel for streaming partial replies emitted by the profile via `ILINK_PARTIAL:` stdout lines.
    // Each chunk is forwarded to Hub immediately so users see incremental output.
    let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<String>();

    // Spawn a forwarding task that sends each partial chunk to Hub as it arrives.
    // This task exits naturally when `partial_tx` is dropped (i.e. when run_cli returns).
    let fwd_client = client.clone();
    let fwd_ctx = ctx.clone();
    let fwd_from_user = from_user.clone();
    let fwd_session_name = session_name_for_cli.clone();
    let forward_handle = tokio::spawn(async move {
        while let Some(chunk) = partial_rx.recv().await {
            let mut req =
                SendMessageRequest::reply_text(fwd_ctx.clone(), chunk, &fwd_from_user, None);
            if let Some(ref mut msg) = req.msg {
                use crate::ilink::types::HubExt;
                let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                ext.session_name = Some(fwd_session_name.clone());
            }
            if let Err(e) = fwd_client.sendmessage(req).await {
                warn!(error = %e, "failed to send partial reply");
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
        partial_tx, // consumed here; drop signals forwarding task to finish
    )
    .await;

    // Wait for all in-flight partial sends to complete before processing the final result.
    let _ = forward_handle.await;

    match cli_result {
        Ok((raw_body, cli_session)) => {
            let body = truncate_chars(
                &raw_body,
                profile.max_reply_chars,
                &profile.truncation_suffix,
            );
            // When body is empty (all content sent via ILINK_PARTIAL) but a new cli_session_id
            // was returned, we still need to notify Hub so it can persist the session UUID.
            // Without this, the session slot stays empty and subsequent quote-replies cannot
            // resume the Claude session (they start a fresh conversation instead).
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
                            use crate::ilink::types::HubExt;
                            let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                            hub_ext.session_name = Some(session_name_for_cli.clone());
                        }
                        if let Err(e) = client.sendmessage(req).await {
                            warn!(error = %e, "failed to persist cli_session_id after ILINK_PARTIAL-only reply");
                        }
                    }
                }
                return Ok(());
            }
            let mut req = SendMessageRequest::reply_text(ctx, body, &from_user, cli_session);
            // Echo back session_name so the Hub uses the correct session for footer labeling
            // even when the user switched sessions between sending the message and the AI reply
            // arriving (race condition fix).
            if let Some(ref mut msg) = req.msg {
                use crate::ilink::types::HubExt;
                let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                hub_ext.session_name = Some(session_name_for_cli.clone());
            }
            client.sendmessage(req).await.context("sendmessage reply")?;
        }
        Err(e) => {
            if app.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let req = SendMessageRequest::reply(ctx, err_text, &from_user);
                if let Err(send_e) = client.sendmessage(req).await {
                    warn!(error = %send_e, "failed to send error reply");
                }
            }
            let err_str = e.to_string().to_lowercase();
            if AUTH_ERROR_KEYWORDS.iter().any(|&k| err_str.contains(k))
                || err_str.contains("not found")
                || err_str.contains("no such file")
            {
                error!("Fatal CLI error (auth/missing): {e}. Exiting bridge process.");
                std::process::exit(1);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// If the first line of `stdout` starts with `prefix`, the remainder of that line is the CLI session id
/// (returned as `Some`); the rest of `stdout` (following lines) is the reply body. If `prefix` is empty
/// or the first line does not match, returns `(stdout, None)`.
fn split_cli_session_from_stdout(prefix: &str, stdout: &str) -> (String, Option<String>) {
    if prefix.is_empty() {
        return (stdout.to_string(), None);
    }
    let mut lines = stdout.lines();
    let Some(first) = lines.next() else {
        return (stdout.to_string(), None);
    };
    if let Some(rest) = first.strip_prefix(prefix) {
        let sid = rest.trim();
        if sid.is_empty() {
            return (stdout.to_string(), None);
        }
        let rest_lines: String = lines.collect::<Vec<_>>().join("\n");
        return (rest_lines, Some(sid.to_string()));
    }
    (stdout.to_string(), None)
}

#[allow(clippy::too_many_arguments)]
async fn run_cli(
    cfg: &BridgeProfile,
    profile_name: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
    from_user: &str,
    context_token: &str,
    partial_tx: mpsc::UnboundedSender<String>,
) -> Result<(String, Option<String>)> {
    let args: Vec<String> = cfg
        .args
        .iter()
        .map(|a| apply_placeholders(a, message, session_id, session_name))
        .collect();

    let command = resolve_spawn_command(&cfg.command);

    let mut cmd = Command::new(&command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &cfg.cwd {
        let dir = expand_user_path(&apply_placeholders(dir, message, session_id, session_name));
        cmd.current_dir(&dir);
    }

    // P0: always inject ILINK_* env vars so any profile script/SDK can read them without
    // requiring explicit `env:` entries in the YAML. User-defined `env:` entries below
    // can override these defaults.
    cmd.env("ILINK_MESSAGE", message);
    cmd.env("ILINK_SESSION_ID", session_id);
    cmd.env("ILINK_SESSION_NAME", session_name);
    cmd.env("ILINK_FROM_USER", from_user);
    cmd.env("ILINK_CONTEXT_TOKEN", context_token);
    cmd.env("ILINK_STREAMING", if cfg.streaming { "1" } else { "0" });

    for (k, v) in &cfg.env {
        let v = apply_placeholders(v, message, session_id, session_name);
        let v = crate::bridge::config::expand_env_var_named(
            &v,
            &std::env::vars().collect(),
            Some(profile_name),
            Some(&format!("env.{k}")),
        )
        .with_context(|| format!("expand env var `{k}` for profile `{profile_name}`"))?;
        cmd.env(k, v);
    }

    match cfg.stdin {
        StdinMode::None => {
            cmd.stdin(std::process::Stdio::null());
        }
        StdinMode::Message => {
            cmd.stdin(std::process::Stdio::piped());
        }
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{command}`"))?;

    let dur = Duration::from_secs(cfg.timeout_secs.max(1));

    // Take stdout/stderr handles before any awaits so they are not moved into futures below.
    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in a background task to prevent the stderr pipe buffer from filling up and
    // blocking the subprocess (which could also stall stdout writes).
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        tokio::io::BufReader::new(child_stderr)
            .read_to_end(&mut buf)
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Write stdin concurrently with reading stdout to avoid pipe deadlock when both pipes are full.
    let stdin_task: Option<tokio::task::JoinHandle<Result<()>>> =
        if matches!(cfg.stdin, StdinMode::Message) {
            let mut stdin = child
                .stdin
                .take()
                .context("stdin pipe missing for stdin: message")?;
            let message_owned = message.to_string();
            Some(tokio::spawn(async move {
                stdin
                    .write_all(message_owned.as_bytes())
                    .await
                    .context("write stdin")?;
                stdin.shutdown().await.context("shutdown stdin")?;
                Ok(())
            }))
        } else {
            None
        };

    // Stream stdout line by line.  Lines prefixed with `ILINK_PARTIAL:` carry a JSON-encoded
    // text chunk that should be forwarded to the WeChat user immediately; all other lines
    // accumulate as the final reply body (existing P0 semantics).
    // When `cfg.streaming` is false, ILINK_PARTIAL lines are NOT forwarded — instead the
    // decoded text is appended to final_lines so the complete response is sent at the end.
    let streaming = cfg.streaming;
    let stream_result: Result<Vec<String>> =
        tokio::time::timeout(dur, async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(child_stdout);
            let mut final_lines: Vec<String> = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.context("read stdout")?;
                if n == 0 {
                    break; // EOF — subprocess exited
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if let Some(json_part) = trimmed.strip_prefix("ILINK_PARTIAL:") {
                    if streaming {
                        match serde_json::from_str::<String>(json_part) {
                            Ok(chunk) => {
                                let _ = partial_tx.send(chunk);
                            }
                            Err(e) => {
                                warn!(error = %e, raw = %json_part, "failed to decode ILINK_PARTIAL chunk; skipping");
                            }
                        }
                    }
                    // When streaming is disabled, discard ILINK_PARTIAL lines entirely.
                    // The built-in profile (claude_code.rs) will emit the full text via
                    // a non-ILINK_PARTIAL line (stdout) when ILINK_STREAMING=0.
                } else {
                    final_lines.push(line.clone());
                }
            }
            // Drop partial_tx here so the forwarding task observes channel close after
            // all chunks have been sent (or immediately when streaming is disabled).
            drop(partial_tx);
            Ok(final_lines)
        })
        .await
        .map_err(|_| anyhow::anyhow!("CLI timed out after {}s", cfg.timeout_secs))?;

    let final_lines = stream_result?;

    // Wait for subprocess exit (it should already have exited since we got EOF on stdout).
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| anyhow::anyhow!("CLI failed to exit after stdout EOF"))?
        .context("wait for CLI process")?;

    // Collect stdin write result (non-fatal).
    if let Some(task) = stdin_task {
        match task.await {
            Ok(Err(e)) => warn!(error = %e, "stdin write error (non-fatal)"),
            Err(e) => warn!(error = %e, "stdin task panicked"),
            Ok(Ok(())) => {}
        }
    }

    // Collect stderr.
    let stderr = stderr_task.await.unwrap_or_default();
    if !stderr.is_empty() {
        tracing::debug!(stderr = %stderr, "CLI stderr");
    }

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let stdout_str: String = final_lines.concat();
        anyhow::bail!(
            "command exited with status {code}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout_str}"
        );
    }

    let mut stdout = final_lines.concat();

    if cfg.include_stderr_in_reply && !stderr.is_empty() {
        stdout.push_str("\n--- stderr ---\n");
        stdout.push_str(&stderr);
    }

    let prefix = cfg
        .cli_session_first_line_prefix
        .as_deref()
        .unwrap_or("")
        .trim();
    let (body, cli_sid) = split_cli_session_from_stdout(prefix, &stdout);
    Ok((body, cli_sid))
}

/// Replace `{{MESSAGE}}`, `{{SESSION_ID}}`, and `{{SESSION_NAME}}` in a template string.
///
/// - `{{MESSAGE}}` — the user's text (prefix stripped when using prefix routing)
/// - `{{SESSION_ID}}` — Hub-persisted backend session UUID (e.g. for `claude --resume`)
/// - `{{SESSION_NAME}}` — human-readable session name (e.g. `"feature-a"`, default `"default"`)
fn apply_placeholders(
    template: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
) -> String {
    template
        .replace("{{MESSAGE}}", message)
        .replace("{{SESSION_ID}}", session_id)
        .replace("{{SESSION_NAME}}", session_name)
}

#[cfg(windows)]
const BRIDGE_BINARY_FILE: &str = "ilink-hub-bridge.exe";
#[cfg(not(windows))]
const BRIDGE_BINARY_FILE: &str = "ilink-hub-bridge";

/// Resolve the `ilink-hub-bridge` executable for spawning child bridge processes.
///
/// When the current process is already `ilink-hub-bridge`, returns `current_exe()`.
/// Otherwise checks `ILINKHUB_BRIDGE_EXE`, a sibling binary next to `current_exe`,
/// then `PATH`. Falls back to the bare command name.
pub fn resolve_bridge_executable() -> PathBuf {
    if let Ok(override_path) = std::env::var("ILINKHUB_BRIDGE_EXE") {
        let trimmed = override_path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Ok(current) = std::env::current_exe() {
        if is_bridge_executable(&current) {
            return current;
        }
        if let Some(sibling) = sibling_bridge_executable(&current) {
            return sibling;
        }
    }

    if let Some(from_path) = find_in_path(BRIDGE_BINARY_FILE) {
        return from_path;
    }

    PathBuf::from(BRIDGE_BINARY_FILE)
}

fn is_bridge_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|name| {
            #[cfg(windows)]
            {
                name.eq_ignore_ascii_case("ilink-hub-bridge.exe")
            }
            #[cfg(not(windows))]
            {
                name == "ilink-hub-bridge"
            }
        })
        .unwrap_or(false)
}

fn sibling_bridge_executable(current: &Path) -> Option<PathBuf> {
    let dir = current.parent()?;
    let sibling = dir.join(BRIDGE_BINARY_FILE);
    if sibling.is_file() {
        Some(sibling)
    } else {
        None
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

/// Resolve the executable for built-in self-invocation (`ilink-hub-bridge profile …`).
fn resolve_spawn_command(command: &str) -> String {
    if command == "ilink-hub-bridge" {
        return resolve_bridge_executable().to_string_lossy().into_owned();
    }
    command.to_string()
}

fn truncate_chars(s: &str, max_chars: usize, suffix: &str) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let budget = max_chars.saturating_sub(suffix.chars().count());
    s.chars().take(budget).collect::<String>() + suffix
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, thiserror::Error)]
#[serde(rename_all = "camelCase")]
pub enum ProbeError {
    #[error("未找到 `{0}` 命令，请先安装该 CLI 工具并确保其在 PATH 中")]
    NotFound(String),
    #[error("项目目录不存在: {0}")]
    ConfigError(String),
    #[error("未认证，请先登录 CLI: {0}")]
    Unauthenticated(String),
    #[error("CLI 执行失败: {0}")]
    ExecutionError(String),
}

impl ProbeError {
    pub fn error_type(&self) -> &'static str {
        match self {
            ProbeError::NotFound(_) => "NotFound",
            ProbeError::ConfigError(_) => "ConfigError",
            ProbeError::Unauthenticated(_) => "Unauthenticated",
            ProbeError::ExecutionError(_) => "ExecutionError",
        }
    }
}

pub fn find_in_path_robust(name: &str) -> Option<PathBuf> {
    if let Some(path) = find_in_path(name) {
        return Some(path);
    }
    // Fallback common search directories on macOS/Linux
    #[cfg(not(windows))]
    {
        let common_dirs = [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ];
        for &dir in &common_dirs {
            let candidate = std::path::Path::new(dir).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // Also check common user-local bin directories
        if let Ok(home) = std::env::var("HOME") {
            let home_path = std::path::Path::new(&home);
            for rel in &[".local/bin", ".npm-global/bin", ".cargo/bin"] {
                let candidate = home_path.join(rel).join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

pub fn check_command_exists(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains('/') || command.contains('\\') {
        let path = std::path::Path::new(command);
        return path.exists() && path.is_file();
    }
    find_in_path_robust(command).is_some()
}

pub fn probe_profile_light(profile: &BridgeProfile) -> Result<(), ProbeError> {
    if let Some(dir) = &profile.cwd {
        let expanded_dir = crate::paths::expand_user_path(
            &dir.replace("{{MESSAGE}}", "ping")
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        let path = std::path::Path::new(&expanded_dir);
        if !path.exists() {
            return Err(ProbeError::ConfigError(expanded_dir));
        }
    }

    // Map built-in profile types to the underlying CLI binary they require.
    let command_to_check: &str = match profile.profile_type.as_deref() {
        Some("claude-code") => "claude",
        Some("codex") => "codex",
        Some("cursor") => "cursor",
        Some("agy") => "agy",
        _ => {
            // For external commands, also map known wrapper names to real binaries.
            match profile.command.as_str() {
                "claude" => "claude",
                "codex" => "codex",
                "cursor" => "cursor",
                "agy" => "agy",
                other => other,
            }
        }
    };

    if !check_command_exists(command_to_check) {
        return Err(ProbeError::NotFound(command_to_check.to_string()));
    }

    Ok(())
}

pub async fn dry_run_profile(profile: &BridgeProfile, message: &str) -> Result<String, ProbeError> {
    if let Some(dir) = &profile.cwd {
        let expanded_dir = crate::paths::expand_user_path(
            &dir.replace("{{MESSAGE}}", message)
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        let path = std::path::Path::new(&expanded_dir);
        if !path.exists() {
            return Err(ProbeError::ConfigError(expanded_dir));
        }
    }

    // When the profile self-invokes `ilink-hub-bridge profile <type>`, resolve the
    // underlying CLI binary so the dry-run actually exercises that tool.
    // If the profile already specifies a concrete command (e.g. in tests using `echo`),
    // use it as-is so mock tests continue to work.
    let command = if profile.command == "ilink-hub-bridge" {
        match profile.profile_type.as_deref() {
            Some("claude-code") => "claude".to_string(),
            Some("codex") => "codex".to_string(),
            Some("cursor") => "cursor".to_string(),
            Some("agy") => "agy".to_string(),
            _ => profile.command.clone(),
        }
    } else {
        profile.command.clone()
    };

    if !check_command_exists(&command) {
        return Err(ProbeError::NotFound(command));
    }

    // Resolve to absolute path for well-known coding-agent CLIs.
    const KNOWN_AGENT_CLIS: &[&str] = &["claude", "agent", "codex", "cursor", "agy"];
    let resolved_command = if KNOWN_AGENT_CLIS.contains(&command.as_str()) {
        find_in_path_robust(&command)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(command)
    } else {
        command
    };

    // Build the dry-run arg list that mirrors each built-in's actual invocation.
    // This is keyed on `profile_type` so that test mocks (which use `echo` as the
    // command) still produce the expected output with the real CLI's args.
    let args: Vec<String> = match profile.profile_type.as_deref() {
        Some("claude-code") => vec![
            "--output-format".into(),
            "json".into(),
            "--dangerously-skip-permissions".into(),
            "-p".into(),
            message.to_string(),
        ],
        Some("codex") => vec![
            "--approval-mode".into(),
            "full-auto".into(),
            "-q".into(),
            message.to_string(),
        ],
        Some("cursor") => vec![
            "agent".into(),
            "run".into(),
            "--prompt".into(),
            message.to_string(),
        ],
        Some("agy") => vec![
            "--dangerously-skip-permissions".into(),
            "-p".into(),
            message.to_string(),
        ],
        _ => profile
            .args
            .iter()
            .map(|a| {
                a.replace("{{MESSAGE}}", message)
                    .replace("{{SESSION_ID}}", "")
                    .replace("{{SESSION_NAME}}", "default")
            })
            .collect(),
    };

    let mut cmd = tokio::process::Command::new(&resolved_command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &profile.cwd {
        let expanded_dir = crate::paths::expand_user_path(
            &dir.replace("{{MESSAGE}}", message)
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        cmd.current_dir(&expanded_dir);
    }

    cmd.env("ILINK_MESSAGE", message);
    cmd.env("ILINK_SESSION_ID", "");
    cmd.env("ILINK_SESSION_NAME", "default");
    cmd.env("ILINK_FROM_USER", "probe");
    cmd.env("ILINK_CONTEXT_TOKEN", "probe");

    for (k, v) in &profile.env {
        let v = v
            .replace("{{MESSAGE}}", message)
            .replace("{{SESSION_ID}}", "")
            .replace("{{SESSION_NAME}}", "default");
        cmd.env(k, v);
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(ProbeError::NotFound(resolved_command));
            }
            return Err(ProbeError::ExecutionError(format!("无法启动进程: {e}")));
        }
    };

    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
            .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(ProbeError::ExecutionError(format!("等待进程退出失败: {e}"))),
            Err(_) => return Err(ProbeError::ExecutionError("执行超时 (10s)".to_string())),
        };

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let all_output = format!("{}\n{}", stdout_str, stderr_str).to_lowercase();
        if AUTH_ERROR_KEYWORDS.iter().any(|&k| all_output.contains(k)) {
            return Err(ProbeError::Unauthenticated(format!(
                "exit code: {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
                output.status.code(),
                stderr_str,
                stdout_str
            )));
        }
        return Err(ProbeError::ExecutionError(format!(
            "exit code: {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
            output.status.code(),
            stderr_str,
            stdout_str
        )));
    }

    Ok(stdout_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_message_session_id_and_name() {
        assert_eq!(
            apply_placeholders(
                "{{MESSAGE}}|{{SESSION_ID}}|{{SESSION_NAME}}",
                "hi",
                "sid-9",
                "feat-a"
            ),
            "hi|sid-9|feat-a"
        );
    }

    #[test]
    fn placeholder_session_name_defaults_to_default() {
        assert_eq!(
            apply_placeholders("name={{SESSION_NAME}}", "", "", "default"),
            "name=default"
        );
    }

    #[test]
    fn split_cli_session_first_line() {
        let (body, sid) =
            split_cli_session_from_stdout("ILINK_SESSION:", "ILINK_SESSION:uuid-1\nhello\n");
        assert_eq!(sid.as_deref(), Some("uuid-1"));
        assert_eq!(body, "hello");
    }

    #[test]
    fn split_cli_session_no_match_returns_full() {
        let (body, sid) = split_cli_session_from_stdout("ILINK_SESSION:", "plain\n");
        assert!(sid.is_none());
        assert_eq!(body, "plain\n");
    }

    #[test]
    fn truncate_respects_chars() {
        let s = truncate_chars("abcde", 4, "…");
        assert_eq!(s, "abc…");
    }

    #[test]
    fn resolve_spawn_command_uses_bridge_executable_for_self_invoke() {
        let resolved = resolve_spawn_command("ilink-hub-bridge");
        assert_eq!(resolved, resolve_bridge_executable().to_string_lossy());
    }

    #[test]
    fn resolve_spawn_command_passthrough_other_commands() {
        assert_eq!(resolve_spawn_command("claude"), "claude");
    }

    #[test]
    fn resolve_bridge_executable_prefers_current_exe_when_already_bridge() {
        if let Ok(exe) = std::env::current_exe() {
            if is_bridge_executable(&exe) {
                assert_eq!(resolve_bridge_executable(), exe);
            }
        }
    }

    #[test]
    fn resolve_bridge_executable_falls_back_to_command_name() {
        if std::env::var_os("ILINKHUB_BRIDGE_EXE").is_some() {
            return;
        }
        if let Ok(exe) = std::env::current_exe() {
            if is_bridge_executable(&exe) || sibling_bridge_executable(&exe).is_some() {
                return;
            }
            if find_in_path(BRIDGE_BINARY_FILE).is_some() {
                return;
            }
        }
        assert_eq!(
            resolve_bridge_executable(),
            PathBuf::from(BRIDGE_BINARY_FILE)
        );
    }

    #[tokio::test]
    async fn test_stdin_write_timeout() {
        // Use absolute paths to avoid PATH-lookup failures in sandboxed CI environments.
        // macOS: /bin/sleep  Linux: /usr/bin/sleep  (both ship with coreutils).
        let sleep_cmd = if cfg!(target_os = "macos") {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let yaml =
            format!("command: {sleep_cmd}\nargs: [\"10\"]\nstdin: message\ntimeout_secs: 1\n");
        let app = BridgeApp::parse_yaml(&yaml).unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        // Create a large message that will fill the OS pipe buffer and block if not read
        let large_msg = "A".repeat(128 * 1024);

        let start = std::time::Instant::now();
        let (partial_tx, _partial_rx) = mpsc::unbounded_channel::<String>();
        let res = run_cli(
            profile,
            "test_profile",
            &large_msg,
            "session-123",
            "session-name",
            "user-123",
            "ctx-123",
            partial_tx,
        )
        .await;

        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "Expected stdin write to timeout, but it succeeded: {:?}",
            res
        );
        let err_msg = res.unwrap_err().to_string();
        // Accept "timed out" / "stdin" (normal timeout path) or "spawn" / "No such file"
        // (CI environment where the sleep binary is absent). All are valid error propagation.
        assert!(
            err_msg.contains("timed out")
                || err_msg.contains("stdin")
                || err_msg.contains("spawn")
                || err_msg.contains("No such file"),
            "Expected timeout or spawn error, got: {}",
            err_msg
        );
        assert!(elapsed.as_secs() < 3, "Took too long: {:?}", elapsed);
    }

    #[tokio::test]
    async fn test_probe_profile_light_and_dry_run() {
        let profile_missing_cwd = BridgeProfile {
            command: "echo".to_string(),
            cwd: Some("/nonexistent-path-for-sure-12345".to_string()),
            ..Default::default()
        };
        let err = probe_profile_light(&profile_missing_cwd).unwrap_err();
        assert!(matches!(err, ProbeError::ConfigError(_)));

        let profile_missing_cmd = BridgeProfile {
            command: "nonexistent-cli-cmd-12345".to_string(),
            ..Default::default()
        };
        let err2 = probe_profile_light(&profile_missing_cmd).unwrap_err();
        assert!(matches!(err2, ProbeError::NotFound(_)));

        let profile_valid = BridgeProfile {
            command: "echo".to_string(),
            args: vec!["{{MESSAGE}}".to_string()],
            ..Default::default()
        };
        probe_profile_light(&profile_valid).unwrap();

        let res = dry_run_profile(&profile_valid, "hello").await.unwrap();
        assert!(res.contains("hello"));

        let profile_claude_mock = BridgeProfile {
            command: "echo".to_string(),
            profile_type: Some("claude-code".to_string()),
            ..Default::default()
        };
        let res_claude = dry_run_profile(&profile_claude_mock, "ping").await.unwrap();
        assert!(res_claude.contains("--dangerously-skip-permissions"));
        assert!(res_claude.contains("-p"));
        assert!(res_claude.contains("ping"));
    }
}

// ─── SessionDispatcher tests ──────────────────────────────────────────────────
//
// These tests verify the routing / channel-assignment logic of `SessionDispatcher`
// without running real CLI commands or making real HTTP calls.
//
// Strategy:
//   • `fake_client()` points to a loopback port that refuses connections; the worker
//     tasks will fail on `sendmessage` but that does not affect sender-map state.
//   • We inspect `sender_keys()` immediately after `dispatch()` returns, before any
//     async work has time to mutate the map, to verify channel-routing decisions.
#[cfg(test)]
mod dispatcher_tests {
    use super::*;
    use crate::ilink::types::{HubExt, MessageItem, TextItem};

    /// Builds a `WeixinMessage` that looks like a real inbound user message.
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
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }])),
            from_user_id: Some("user1".into()),
            ..Default::default()
        }
    }

    /// A `BridgeApp` whose CLI (`echo`) exits immediately — keeps workers short-lived.
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

    /// `HubClient` pointing at a port that refuses connections.
    /// Worker tasks will fail to send replies, but routing logic is unaffected.
    fn fake_client() -> HubClient {
        HubClient::new("http://127.0.0.1:1".into(), "test-token".into())
    }

    // ── session_dispatch_key ──────────────────────────────────────────────────

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

    // ── SessionDispatcher routing ─────────────────────────────────────────────

    #[tokio::test]
    async fn same_key_reuses_single_sender() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        let msg = make_msg("ctx-a", "default");
        disp.dispatch(msg.clone()).await;
        disp.dispatch(msg.clone()).await;
        // Both messages share one worker channel → exactly one map entry.
        assert_eq!(disp.sender_keys().await, vec!["ctx-a:default"]);
    }

    #[tokio::test]
    async fn different_ctx_tokens_get_separate_senders() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        disp.dispatch(make_msg("ctx-a", "default")).await;
        disp.dispatch(make_msg("ctx-b", "default")).await;
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-a:default", "ctx-b:default"]
        );
    }

    #[tokio::test]
    async fn different_session_names_get_separate_senders() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        disp.dispatch(make_msg("ctx-a", "feature-x")).await;
        disp.dispatch(make_msg("ctx-a", "feature-y")).await;
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-a:feature-x", "ctx-a:feature-y"]
        );
    }

    #[tokio::test]
    async fn three_distinct_sessions_create_three_senders() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        disp.dispatch(make_msg("ctx-1", "default")).await;
        disp.dispatch(make_msg("ctx-2", "default")).await;
        disp.dispatch(make_msg("ctx-1", "feature-a")).await;
        // 3 unique keys: (ctx-1,default), (ctx-2,default), (ctx-1,feature-a)
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-1:default", "ctx-1:feature-a", "ctx-2:default"]
        );
    }

    #[tokio::test]
    async fn repeated_same_key_does_not_grow_sender_map() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        let msg = make_msg("ctx-x", "s1");
        for _ in 0..5 {
            disp.dispatch(msg.clone()).await;
        }
        assert_eq!(disp.sender_keys().await.len(), 1);
    }

    #[tokio::test]
    async fn dead_sender_triggers_new_worker_on_next_dispatch() {
        let disp = SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()));
        let msg = make_msg("ctx-z", "default");

        // First dispatch: inserts a sender and spawns a worker.
        disp.dispatch(msg.clone()).await;

        // Manually close the channel to simulate the worker exiting unexpectedly.
        {
            let mut senders = disp.senders.lock().await;
            senders.remove("ctx-z:default");
        }
        assert_eq!(disp.sender_keys().await.len(), 0);

        // Next dispatch should transparently recreate the sender + worker.
        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys().await, vec!["ctx-z:default"]);
    }
}
