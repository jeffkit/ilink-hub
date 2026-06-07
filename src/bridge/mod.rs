//! CLI bridge: connect to iLink Hub as a virtual-token backend and run a local command per text message.
//! Supports **single-profile YAML** (flat `command` / `args`) or **multi-profile YAML**
//! (`profiles` + `routing`: `fixed` or `prefix`).
//!
//! Used by the `ilink-hub-bridge` binary; see `docs/bridge/README.md`.

mod config;
mod connection;

pub use config::{BridgeApp, BridgeConfig, BridgeProfile, RoutingStrategy, StdinMode};
pub use connection::{default_local_credential_path, resolve_hub_connection};

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, SendMessageRequest, SendMessageResponse,
    WeixinMessage,
};

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

    async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesResponse> {
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
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("getupdates HTTP {status}: {t}");
        }
        let out: GetUpdatesResponse = resp.json().await?;
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        Ok(out)
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

/// Long-poll Hub and dispatch inbound user text to the configured CLI (runs until process exit).
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) {
    let client = HubClient::new(hub_url, token);
    let mut buf = String::new();
    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        let resp = match client.getupdates(&mut buf).await {
            Ok(r) => r,
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

    info!(%profile_name, %profile.command, "running bridge profile");

    match run_cli(profile, &payload, &from_user).await {
        Ok(output) => {
            let body = truncate_chars(&output, profile.max_reply_chars, &profile.truncation_suffix);
            let req = SendMessageRequest::reply(ctx, body, &from_user);
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

async fn run_cli(cfg: &BridgeProfile, message: &str, from_user_id: &str) -> Result<String> {
    let args: Vec<String> = cfg
        .args
        .iter()
        .map(|a| apply_placeholders(a, message, from_user_id))
        .collect();

    let mut cmd = Command::new(&cfg.command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &cfg.cwd {
        cmd.current_dir(dir);
    }

    for (k, v) in &cfg.env {
        cmd.env(k, apply_placeholders(v, message, from_user_id));
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
        .with_context(|| format!("failed to spawn `{}`", cfg.command))?;

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

    Ok(stdout)
}

fn apply_placeholders(template: &str, message: &str, from_user_id: &str) -> String {
    template
        .replace("{{MESSAGE}}", message)
        .replace("{{FROM_USER_ID}}", from_user_id)
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
    fn placeholders() {
        assert_eq!(
            apply_placeholders("x {{MESSAGE}} y {{FROM_USER_ID}}", "hi", "u1"),
            "x hi y u1"
        );
    }

    #[test]
    fn truncate_respects_chars() {
        let s = truncate_chars("abcde", 4, "…");
        assert_eq!(s, "abc…");
    }
}
