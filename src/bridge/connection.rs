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
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::client::{HubPairingClient, HubPairingCredentials, HubPairingOptions};
use crate::ilink::types::{BaseInfo, GetUpdatesRequest, GetUpdatesResponse};

/// Default path for cached downstream credentials (same JSON shape as Hub pairing).
pub fn default_local_credential_path() -> PathBuf {
    crate::paths::default_bridge_credentials_path()
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

async fn write_credentials(
    path: &Path,
    hub_url: &str,
    vtoken: &str,
    client_name: &str,
) -> Result<()> {
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
        client_name: Some(client_name.to_string()),
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

/// Returns `true` when Hub rejects the virtual token (HTTP 401 or `ret == 401`).
pub fn hub_response_token_rejected(status: reqwest::StatusCode, ret: Option<i32>) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || ret == Some(401)
}

/// Probe Hub with a zero-timeout `getupdates` to ensure `token` is registered.
pub async fn validate_hub_token(hub_url: &str, token: &str) -> Result<()> {
    let hub = hub_url.trim().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()
        .context("build HTTP client for token validation")?;
    let url = format!("{hub}/ilink/bot/getupdates");
    let body = GetUpdatesRequest {
        get_updates_buf: String::new(),
        base_info: Some(BaseInfo::default()),
        timeout: Some(0),
    };
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token.trim()))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("probe Hub token via {url}"))?;
    let status = resp.status();
    let out: GetUpdatesResponse = resp.json().await.context("parse getupdates probe")?;
    if hub_response_token_rejected(status, out.ret) {
        let detail = out.errmsg.unwrap_or_else(|| "token rejected".into());
        anyhow::bail!("{detail}");
    }
    if let Some(ret) = out.ret {
        if ret != 0 {
            anyhow::bail!(
                "getupdates probe failed: ret={ret} errmsg={}",
                out.errmsg.as_deref().unwrap_or("unknown")
            );
        }
    }
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

fn auto_client_name(
    explicit: Option<&str>,
    saved: Option<&str>,
    config_path: Option<&Path>,
) -> String {
    if let Some(n) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    if let Some(n) = saved.map(str::trim).filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    default_auto_client_name(config_path)
}

/// Default Hub client name for auto-register: `local-<hostname>` or `local-<hostname>-<config-stem>`.
pub fn default_auto_client_name(config_path: Option<&Path>) -> String {
    let host = sanitize_client_name_segment(&local_hostname());
    match config_path
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
    {
        Some(stem) if !stem.is_empty() => {
            let stem = sanitize_client_name_segment(stem);
            format!("local-{host}-{stem}")
        }
        _ => format!("local-{host}"),
    }
}

fn local_hostname() -> String {
    // Prefer env vars set by the shell or container runtime — no syscall needed.
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    // Read hostname from the kernel's proc interface — no fork, no child process.
    // Fallback chain: /proc/sys/kernel/hostname → /etc/hostname → "host".
    #[cfg(unix)]
    {
        for path in &["/proc/sys/kernel/hostname", "/etc/hostname"] {
            if let Ok(contents) = std::fs::read_to_string(path) {
                let h = contents.trim();
                if !h.is_empty() {
                    return h.to_string();
                }
            }
        }
    }
    "host".to_string()
}

fn sanitize_client_name_segment(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

async fn auto_register_and_save(
    path: &Path,
    hub: &str,
    register_client_name: Option<&str>,
    saved_client_name: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(String, String)> {
    let hub = hub.trim().trim_end_matches('/').to_string();
    let name = auto_client_name(register_client_name, saved_client_name, config_path);
    let label = "auto-registered downstream";
    let vtoken = register_via_hub_http(&hub, &name, label).await.with_context(|| {
        "自动调用 /hub/register 失败。若返回体为业务错误：检查 `ILINK_ADMIN_TOKEN` 是否与 Hub 一致；\
         或改用 `WEIXIN_TOKEN` / `--pair`。"
    })?;

    write_credentials(path, &hub, &vtoken, &name).await?;
    info!(
        path = %path.display(),
        client = %name,
        "registered downstream via /hub/register and saved credentials"
    );
    println!(
        "已向 Hub 自动注册客户端「{name}」（通用 /hub/register）。\
         同名客户端重启后会复用该名称，不会重复堆积。若微信里有多于一个后端，请发送：/use {name}"
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
    config_path: Option<&Path>,
) -> Result<(String, String)> {
    let hub = hub_url.trim().trim_end_matches('/').to_string();

    if let Some(tok) = explicit_token.map(str::trim).filter(|s| !s.is_empty()) {
        validate_hub_token(&hub, tok).await.with_context(|| {
            "WEIXIN_TOKEN / --token 未被当前 Hub 接受（未注册或已失效）。\
                 请用 `ilink-hub register` 或 `ilink-hub-bridge --force-register` 重新注册。"
        })?;
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
            let hub_for_token = if base.is_empty() {
                hub.clone()
            } else {
                base.clone()
            };
            if let Err(e) = validate_hub_token(&hub_for_token, &creds.token).await {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "saved bridge token rejected by Hub; removing credentials and re-registering"
                );
                let saved_name = creds.client_name.as_deref();
                let _ = tokio::fs::remove_file(&path).await;
                return auto_register_and_save(
                    &path,
                    &hub,
                    register_client_name,
                    saved_name,
                    config_path,
                )
                .await;
            }
            if base.is_empty() {
                Ok((hub, creds.token))
            } else {
                Ok((base, creds.token))
            }
        }
        LocalCredState::Missing => {
            auto_register_and_save(&path, &hub, register_client_name, None, config_path).await
        }
        LocalCredState::ExistsUnusable => {
            if force_register {
                let _ = tokio::fs::remove_file(&path).await;
                auto_register_and_save(&path, &hub, register_client_name, None, config_path).await
            } else {
                anyhow::bail!(
                    "凭证文件 {} 已存在但无法使用（内容损坏或 token 为空）。\
                     为避免静默覆盖扫码配对等已有文件，已停止自动注册。\
                     请删除该文件、设置 WEIXIN_TOKEN、使用 `--pair`，或加上 `--force-register` 删除该文件后重新自动注册。",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn hub_response_token_rejected_detects_401() {
        assert!(hub_response_token_rejected(
            reqwest::StatusCode::UNAUTHORIZED,
            None
        ));
        assert!(hub_response_token_rejected(
            reqwest::StatusCode::OK,
            Some(401)
        ));
        assert!(!hub_response_token_rejected(
            reqwest::StatusCode::OK,
            Some(0)
        ));
    }

    #[test]
    fn default_auto_client_name_uses_config_stem() {
        let name = default_auto_client_name(Some(Path::new("/Users/me/ilink-claude.yaml")));
        assert!(name.starts_with("local-"));
        assert!(name.ends_with("-ilink-claude"));
    }

    #[test]
    fn sanitize_client_name_segment_replaces_invalid_chars() {
        assert_eq!(sanitize_client_name_segment("My Mac.local"), "My-Mac-local");
    }

    #[test]
    fn auto_client_name_prefers_explicit_then_saved() {
        assert_eq!(
            auto_client_name(Some("custom"), Some("saved"), None),
            "custom"
        );
        assert_eq!(auto_client_name(None, Some("saved"), None), "saved");
    }
}
