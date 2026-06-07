//! Resolve Hub base URL + virtual token for the local downstream process.
//!
//! Hub only exposes generic `/hub/register` and pairing APIs — no bridge-specific concepts.
//!
//! Resolution order: explicit token → `--pair` (QR) → saved JSON → **HTTP `/hub/register`**.
//! There is **no** automatic fallback to QR or register when the Hub is unreachable: auto-register
//! fails fast with a hint; use `--pair` or `WEIXIN_TOKEN` once a Hub is available.
//!
//! **Credential file safety:** if the credential path already exists but is unusable (invalid JSON
//! or empty token), we **do not** silently overwrite it with auto-register (avoids clobbering a
//! QR-paired file). Use `--force-register` to delete that file and run auto-register again.
//!
//! The bridge binary never requires `ilink-hub` to be installed on the same machine — only a
//! reachable `WEIXIN_BASE_URL` (local or remote).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use crate::client::{HubPairingClient, HubPairingCredentials, HubPairingOptions};

/// Default path for cached downstream credentials (same JSON shape as Hub pairing).
pub fn default_local_credential_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ilink-hub")
        .join("bridge-credentials.json")
}

fn credential_path(cred_file: Option<&str>) -> PathBuf {
    cred_file
        .map(PathBuf::from)
        .unwrap_or_else(default_local_credential_path)
}

enum LocalCredState {
    /// No file at the credential path.
    Missing,
    /// Parsed credentials with a non-empty token.
    Valid(HubPairingCredentials),
    /// File exists but JSON is invalid or token is empty — must not silently auto-overwrite.
    ExistsUnusable,
}

async fn local_credential_state(path: &Path) -> Result<LocalCredState> {
    match tokio::fs::read_to_string(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalCredState::Missing),
        Err(e) => Err(e.into()),
        Ok(data) => match serde_json::from_str::<HubPairingCredentials>(&data) {
            Ok(c) if !c.token.trim().is_empty() => Ok(LocalCredState::Valid(c)),
            Ok(_) => Ok(LocalCredState::ExistsUnusable),
            Err(_) => Ok(LocalCredState::ExistsUnusable),
        },
    }
}

async fn write_credentials(path: &Path, hub_url: &str, vtoken: &str) -> Result<()> {
    let saved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}Z", d.as_secs()))
        .unwrap_or_default();
    let creds = HubPairingCredentials {
        token: vtoken.to_string(),
        base_url: hub_url.trim().trim_end_matches('/').to_string(),
        account_id: "ilink-hub@hub.local".to_string(),
        user_id: "hub-client".to_string(),
        saved_at: Some(saved_at),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let data = serde_json::to_string_pretty(&creds)?;
    tokio::fs::write(path, format!("{data}\n"))
        .await
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

async fn register_via_hub_http(hub_url: &str, name: &str, label: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let url = format!("{}/hub/register", hub_url.trim().trim_end_matches('/'));
    let mut req = client
        .post(&url)
        .json(&serde_json::json!({ "name": name, "label": label }));
    if let Ok(admin) = std::env::var("ILINK_ADMIN_TOKEN") {
        let admin = admin.trim();
        if !admin.is_empty() {
            req = req.header("Authorization", format!("Bearer {admin}"));
        }
    }
    let resp = req
        .send()
        .await
        .with_context(|| {
            format!(
                "无法连接 Hub（{url}）。`ilink-hub-bridge` 不要求本机安装或运行 `ilink-hub`，\
                 只需 `WEIXIN_BASE_URL` 指向**已启动且可达**的 Hub（本机或远程均可）。\
                 若 Hub 未就绪：先启动 Hub 或改 URL；也可改用环境变量 `WEIXIN_TOKEN`，或在 Hub 可用时使用 `ilink-hub-bridge --pair` 扫码配对。"
            )
        })?;
    let resp: serde_json::Value = resp.json().await.context("parse /hub/register response")?;
    if resp["ret"] == 0 {
        let v = resp["vtoken"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("register response missing vtoken")?;
        return Ok(v.to_string());
    }
    let msg = resp["errmsg"].as_str().unwrap_or("unknown");
    anyhow::bail!(
        "POST /hub/register failed: ret={} errmsg={}",
        resp["ret"],
        msg
    )
}

fn auto_client_name(explicit: Option<&str>) -> String {
    if let Some(n) = explicit {
        let n = n.trim();
        if !n.is_empty() {
            return n.to_string();
        }
    }
    format!("local-{}", uuid::Uuid::new_v4().simple())
}

async fn auto_register_and_save(
    path: &Path,
    hub: &str,
    register_client_name: Option<&str>,
) -> Result<(String, String)> {
    let hub = hub.trim().trim_end_matches('/').to_string();
    let name = auto_client_name(register_client_name);
    let label = "auto-registered downstream";
    let vtoken = register_via_hub_http(&hub, &name, label).await.with_context(|| {
        "自动调用 /hub/register 失败。若返回体为业务错误：检查 `ILINK_ADMIN_TOKEN` 是否与 Hub 一致；\
         或改用 `WEIXIN_TOKEN` / `--pair`。"
    })?;

    write_credentials(path, &hub, &vtoken).await?;
    info!(
        path = %path.display(),
        client = %name,
        "registered downstream via /hub/register and saved credentials"
    );
    println!(
        "已向 Hub 自动注册客户端「{name}」（通用 /hub/register）。若微信里有多于一个后端，请发送：/use {name}"
    );

    Ok((hub, vtoken))
}

/// Resolve `(hub_base_url, vtoken)` for the bridge process.
///
/// Order: explicit token → `--pair` QR flow → saved JSON → **POST `/hub/register`** and save.
///
/// If Hub has `ILINK_ADMIN_TOKEN`, set the same variable in the environment when running the bridge
/// so automatic registration can authenticate.
///
/// `force_register`: if the credential file exists but is unusable, delete it and run auto-register.
/// Does not affect a **valid** saved file (that path returns early).
pub async fn resolve_hub_connection(
    hub_url: &str,
    explicit_token: Option<&str>,
    cred_file: Option<&str>,
    force_pair: bool,
    register_client_name: Option<&str>,
    force_register: bool,
) -> Result<(String, String)> {
    let hub = hub_url.trim().trim_end_matches('/').to_string();

    if let Some(tok) = explicit_token.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok((hub, tok.to_string()));
    }

    let path = credential_path(cred_file.map(|s| s.trim()).filter(|s| !s.is_empty()));

    if force_pair {
        let mut opts = HubPairingOptions::new(&hub);
        opts.cred_path = Some(path.to_string_lossy().into_owned());
        opts.force = true;
        let client = HubPairingClient::new(opts);
        let creds = client.pair().await.context("Hub QR pairing")?;
        let base = creds.base_url.trim().trim_end_matches('/').to_string();
        return Ok((base, creds.token));
    }

    match local_credential_state(&path).await? {
        LocalCredState::Valid(creds) => {
            let base = creds.base_url.trim().trim_end_matches('/').to_string();
            if base.is_empty() {
                return Ok((hub, creds.token));
            }
            return Ok((base, creds.token));
        }
        LocalCredState::Missing => {
            return auto_register_and_save(&path, &hub, register_client_name).await
        }
        LocalCredState::ExistsUnusable => {
            if force_register {
                let _ = tokio::fs::remove_file(&path).await;
                return auto_register_and_save(&path, &hub, register_client_name).await;
            }
            anyhow::bail!(
                "凭证文件 {} 已存在但无法使用（内容损坏或 token 为空）。\
                 为避免静默覆盖扫码配对等已有文件，已停止自动注册。\
                 请删除该文件、设置 WEIXIN_TOKEN、使用 `--pair`，或加上 `--force-register` 删除该文件后重新自动注册。",
                path.display()
            );
        }
    }
}
