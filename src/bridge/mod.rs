//! CLI bridge: connect to iLink Hub as a virtual-token backend and run a local command per text message.
//! Supports **single-profile YAML** (flat `command` / `args`) or **multi-profile YAML**
//! (`profiles` + `routing`: `fixed` or `prefix`).
//!
//! Used by the `ilink-hub-bridge` binary; see `docs/bridge/README.md`.

pub mod builtin;
mod config;
mod connection;

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

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
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

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns [`BridgeStop::TokenRejected`] when Hub returns 401 for an unknown/revoked vtoken.
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) -> BridgeStop {
    let client = HubClient::new(hub_url, token);
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

        for msg in resp.msgs.unwrap_or_default() {
            if let Err(e) = handle_one_message(&client, &app, msg).await {
                error!(error = %e, "message handler failed");
            }
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

    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, "running bridge profile");

    match run_cli(
        profile,
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
            let req = SendMessageRequest::reply_text(ctx, body, &from_user, cli_session);
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
        cmd.env(k, apply_placeholders(v, message, session_id, session_name));
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

    if matches!(cfg.stdin, StdinMode::Message) {
        let mut stdin = child
            .stdin
            .take()
            .context("stdin pipe missing for stdin: message")?;
        stdin
            .write_all(message.as_bytes())
            .await
            .context("write stdin")?;
        stdin.shutdown().await.context("shutdown stdin")?;
    }

    let dur = Duration::from_secs(cfg.timeout_secs.max(1));
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

/// Resolve the executable for built-in self-invocation (`ilink-hub-bridge profile …`).
/// Falls back to the bare command name when `current_exe` is unavailable.
fn resolve_spawn_command(command: &str) -> String {
    if command == "ilink-hub-bridge" {
        if let Ok(exe) = std::env::current_exe() {
            return exe.to_string_lossy().into_owned();
        }
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
    fn resolve_spawn_command_uses_current_exe_for_self_invoke() {
        let resolved = resolve_spawn_command("ilink-hub-bridge");
        if let Ok(exe) = std::env::current_exe() {
            assert_eq!(resolved, exe.to_string_lossy());
        }
    }

    #[test]
    fn resolve_spawn_command_passthrough_other_commands() {
        assert_eq!(resolve_spawn_command("claude"), "claude");
    }
}
