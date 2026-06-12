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
async fn run_session_worker(
    key: String,
    mut rx: mpsc::UnboundedReceiver<WeixinMessage>,
    client: HubClient,
    app: Arc<BridgeApp>,
) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = handle_one_message(&client, &app, msg).await {
            error!(session_key = %key, error = %e, "message handler failed");
        }
    }
    info!(session_key = %key, "session worker exiting");
}

/// Routes inbound messages to per-session serial workers.
///
/// Each unique dispatch key owns an `UnboundedSender`; a new Tokio task is spawned lazily
/// on first use. If a worker task exits unexpectedly (channel closed), the next `dispatch`
/// call for that key transparently creates a fresh worker.
struct SessionDispatcher {
    senders: tokio::sync::Mutex<HashMap<String, mpsc::UnboundedSender<WeixinMessage>>>,
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
            Some(tx) => tx.send(msg.clone()).is_err(),
            None => true,
        };

        if needs_new {
            let (tx, rx) = mpsc::unbounded_channel();
            // Cannot fail: receiver is alive and we hold the lock.
            let _ = tx.send(msg);
            senders.insert(key.clone(), tx);
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            tokio::spawn(run_session_worker(key, rx, client, app));
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
    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        let resp = match client.getupdates(&mut buf).await {
            Ok(GetUpdatesOutcome::Ok(r)) => r,
            Ok(GetUpdatesOutcome::TokenRejected) => return BridgeStop::TokenRejected,
            Err(e) => {
                error!(error = %e, "getupdates failed; retrying in 3s");
                tokio::time::sleep(Duration::from_secs(3)).await;
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

    match run_cli(
        profile,
        profile_name,
        &payload,
        &session_for_cli,
        &session_name_for_cli,
        &from_user,
        &ctx,
    )
    .await
    {
        Ok((raw_body, cli_session)) => {
            let body = truncate_chars(
                &raw_body,
                profile.max_reply_chars,
                &profile.truncation_suffix,
            );
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

async fn run_cli(
    cfg: &BridgeProfile,
    profile_name: &str,
    message: &str,
    session_id: &str,
    session_name: &str,
    from_user: &str,
    context_token: &str,
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

    if matches!(cfg.stdin, StdinMode::Message) {
        let mut stdin = child
            .stdin
            .take()
            .context("stdin pipe missing for stdin: message")?;

        let write_fut = async {
            stdin
                .write_all(message.as_bytes())
                .await
                .context("write stdin")?;
            stdin.shutdown().await.context("shutdown stdin")?;
            Ok::<(), anyhow::Error>(())
        };

        tokio::time::timeout(dur, write_fut).await.map_err(|_| {
            anyhow::anyhow!("CLI stdin write timed out after {}s", cfg.timeout_secs)
        })??;
    }

    let output = tokio::time::timeout(dur, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("CLI timed out after {}s", cfg.timeout_secs))?
        .context("wait_with_output")?;

    let status = output.status;
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !stderr.is_empty() {
        tracing::debug!(stderr = %stderr, "CLI stderr");
    }

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        anyhow::bail!(
            "command exited with status {code}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}"
        );
    }

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
        let app = BridgeApp::parse_yaml(
            r#"
command: /bin/sleep
args: ["5"]
stdin: message
timeout_secs: 1
"#,
        )
        .unwrap();
        let (_name, profile, _payload) = app.resolve("hello").unwrap();

        // Create a large message that will fill the OS pipe buffer and block if not read
        let large_msg = "A".repeat(128 * 1024);

        let start = std::time::Instant::now();
        let res = run_cli(
            profile,
            "test_profile",
            &large_msg,
            "session-123",
            "session-name",
            "user-123",
            "ctx-123",
        )
        .await;

        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "Expected stdin write to timeout, but it succeeded: {:?}",
            res
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("timed out") || err_msg.contains("stdin"),
            "Expected timeout error message, got: {}",
            err_msg
        );
        assert!(elapsed.as_secs() < 3, "Took too long: {:?}", elapsed);
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
            item_list: Some(vec![MessageItem {
                item_type: Some(1),
                text_item: Some(TextItem {
                    text: Some("hello".into()),
                }),
                extra: serde_json::Value::Object(Default::default()),
                voice_item: None,
            }]),
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
