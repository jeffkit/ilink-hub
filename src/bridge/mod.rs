//! CLI bridge: connect to iLink Hub as a virtual-token backend and run a local command per text message.
//!
//! Used by the `ilink-hub-bridge` binary; see `docs/bridge/README.md`.

mod connection;

pub use connection::{default_local_credential_path, resolve_hub_connection};

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, SendMessageRequest, SendMessageResponse,
    WeixinMessage,
};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StdinMode {
    /// Do not write stdin (use `{{MESSAGE}}` in args, or a fixed prompt).
    None,
    /// Write the inbound message text to stdin (UTF-8).
    Message,
}

impl Default for StdinMode {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    /// Executable name or path (e.g. `claude`, `/usr/local/bin/codex`).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: StdinMode,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_reply_chars")]
    pub max_reply_chars: usize,
    #[serde(default = "default_truncation_suffix")]
    pub truncation_suffix: String,
    /// Ignore messages that look like bot replies (`message_type == 2`).
    #[serde(default = "default_true")]
    pub skip_bot_messages: bool,
    /// If true, non-text inbound messages are ignored (no reply).
    #[serde(default = "default_true")]
    pub require_text: bool,
    /// On CLI failure, send a short error text back to WeChat.
    #[serde(default = "default_true")]
    pub send_error_reply: bool,
    /// Append stderr to the reply body after stdout (only on success).
    #[serde(default)]
    pub include_stderr_in_reply: bool,
}

fn default_timeout_secs() -> u64 {
    300
}

fn default_max_reply_chars() -> usize {
    8000
}

fn default_truncation_suffix() -> String {
    "\n\n…(输出已截断)".to_string()
}

fn default_true() -> bool {
    true
}

impl BridgeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: BridgeConfig =
            serde_yaml::from_str(&raw).with_context(|| format!("parse YAML {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            anyhow::bail!("`command` must not be empty");
        }
        Ok(())
    }
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
pub async fn run_bridge(hub_url: String, token: String, config: BridgeConfig) {
    let client = HubClient::new(hub_url, token);
    let mut buf = String::new();
    info!(
        command = %config.command,
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
            if let Err(e) = handle_one_message(&client, &config, msg).await {
                error!(error = %e, "message handler failed");
            }
        }
    }
}

async fn handle_one_message(
    client: &HubClient,
    cfg: &BridgeConfig,
    msg: WeixinMessage,
) -> Result<()> {
    if cfg.skip_bot_messages && msg.message_type == Some(2) {
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.to_string(),
        None if !cfg.require_text => String::new(),
        None => return Ok(()),
    };
    if text.trim().is_empty() && cfg.require_text {
        return Ok(());
    }

    let ctx = msg
        .context_token
        .clone()
        .filter(|s| !s.is_empty())
        .context("inbound message missing context_token")?;
    let from_user = msg.from_user_id.clone().unwrap_or_default();

    match run_cli(cfg, &text, &from_user).await {
        Ok(output) => {
            let body = truncate_chars(&output, cfg.max_reply_chars, &cfg.truncation_suffix);
            let req = SendMessageRequest::reply(ctx, body, &from_user);
            client.sendmessage(req).await.context("sendmessage reply")?;
        }
        Err(e) => {
            if cfg.send_error_reply {
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

async fn run_cli(cfg: &BridgeConfig, message: &str, from_user_id: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_yaml() {
        let y = r#"
command: echo
args: ["{{MESSAGE}}"]
stdin: none
"#;
        let c: BridgeConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(c.command, "echo");
        assert_eq!(c.args, vec!["{{MESSAGE}}"]);
        assert_eq!(c.stdin, StdinMode::None);
    }

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
