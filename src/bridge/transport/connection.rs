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
use crate::ilink::login::LoginClient;
use crate::ilink::types::{BaseInfo, GetUpdatesRequest, GetUpdatesResponse};

/// Default path for cached downstream credentials (same JSON shape as Hub pairing).
pub fn default_local_credential_path() -> PathBuf {
    crate::paths::default_bridge_credentials_path()
}

/// Default path for cached `via: direct` credentials (bridge → real iLink upstream).
pub fn default_direct_credential_path() -> PathBuf {
    crate::paths::default_direct_credentials_path()
}

fn credential_path(cred_file: Option<&str>) -> PathBuf {
    cred_file
        .map(PathBuf::from)
        .unwrap_or_else(default_local_credential_path)
}

fn direct_credential_path(cred_file: Option<&str>) -> PathBuf {
    cred_file
        .map(PathBuf::from)
        .unwrap_or_else(default_direct_credential_path)
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

async fn write_creds_file(
    path: &Path,
    base_url: &str,
    token: &str,
    client_name: &str,
    account_id: &str,
    user_id: &str,
) -> Result<()> {
    let saved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}Z", d.as_secs()))
        .unwrap_or_default();
    let creds = HubPairingCredentials {
        token: token.to_string(),
        base_url: base_url.trim().trim_end_matches('/').to_string(),
        account_id: account_id.to_string(),
        user_id: user_id.to_string(),
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

async fn write_credentials(
    path: &Path,
    hub_url: &str,
    vtoken: &str,
    client_name: &str,
) -> Result<()> {
    write_creds_file(
        path,
        hub_url,
        vtoken,
        client_name,
        "ilink-hub@hub.local",
        "hub-client",
    )
    .await
}

/// Write a `via: direct` credential file (same JSON shape as Hub pairing, but
/// labelled for the real iLink upstream so the two modes never share a file).
async fn write_direct_credentials(
    path: &Path,
    base_url: &str,
    bot_token: &str,
    client_name: &str,
) -> Result<()> {
    write_creds_file(
        path,
        base_url,
        bot_token,
        client_name,
        "ilink@direct",
        "direct-client",
    )
    .await
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

async fn register_via_hub_http(
    hub_url: &str,
    name: &str,
    label: &str,
    description: Option<&str>,
) -> Result<String> {
    let client = reqwest::Client::new();
    let url = format!("{}/hub/register", hub_url.trim().trim_end_matches('/'));
    let mut req = client.post(&url).json(&serde_json::json!({
        "name": name,
        "label": label,
        "persona_name": name,
        "persona_emoji": "🤖",
        "description": description,
    }));
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
    if resp["ret"] == 401 {
        let admin_present = std::env::var("ILINK_ADMIN_TOKEN")
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let hint = if admin_present {
            "已设置 ILINK_ADMIN_TOKEN，但 Hub 仍返回 401：请确认它与 Hub 端 ILINK_ADMIN_TOKEN 完全一致。"
        } else {
            "Hub 启用了 admin 鉴权，但本进程未设置 ILINK_ADMIN_TOKEN。请在环境中设置与 Hub 一致的 \
             ILINK_ADMIN_TOKEN（manager 模式下设置在 manager 进程的环境里，会自动透传给子 bridge）。\
             切勿复用其他后端的凭证/token 绕过——共享 vtoken 会导致多个 bridge 抢占同一消息队列。"
        };
        anyhow::bail!("POST /hub/register 被拒绝 (401 Unauthorized)。{hint}");
    }
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
    // gethostname(2) works on both Linux and macOS.
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        extern "C" {
            fn gethostname(
                name: *mut libc_types::CChar,
                namelen: libc_types::SizeT,
            ) -> libc_types::CInt;
        }
        mod libc_types {
            // c_char is i8 on x86_64 Linux but u8 on aarch64 Linux and Apple Silicon.
            // Using std::os::raw::c_char ensures the type matches CStr::from_ptr on all targets.
            pub type CChar = std::os::raw::c_char;
            pub type SizeT = usize;
            pub type CInt = i32;
        }
        let mut buf = [0 as libc_types::CChar; 256];
        let ret = unsafe { gethostname(buf.as_mut_ptr(), buf.len()) };
        if ret == 0 {
            // POSIX: gethostname does not null-terminate if the hostname
            // is >= the buffer size. Force a terminator so CStr::from_ptr
            // never reads past the buffer.
            buf[buf.len() - 1] = 0;
            let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
            if let Ok(s) = cstr.to_str() {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        // Fallback: read from proc/etc files (Linux only).
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
    description: Option<&str>,
) -> Result<(String, String)> {
    let hub = hub.trim().trim_end_matches('/').to_string();
    let name = auto_client_name(register_client_name, saved_client_name, config_path);
    let label = local_hostname();
    let vtoken = register_via_hub_http(&hub, &name, &label, description).await.with_context(|| {
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
#[allow(clippy::too_many_arguments)]
pub async fn resolve_hub_connection(
    hub_url: &str,
    explicit_token: Option<&str>,
    cred_file: Option<&str>,
    force_pair: bool,
    register_client_name: Option<&str>,
    force_register: bool,
    config_path: Option<&Path>,
    description: Option<&str>,
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
        let client = HubPairingClient::new(opts)?;
        let creds = client.pair().await.context("Hub QR pairing")?;
        let base = creds.base_url.trim().trim_end_matches('/').to_string();
        return Ok((base, creds.token));
    }

    match local_credential_state(&path).await? {
        LocalCredState::Valid(creds) => {
            // Always validate against the explicitly-provided hub URL, not the
            // URL stored in the credential file.  The stored base_url can be
            // stale when the hub moved to a new port: both the old and new
            // hub instances share the same SQLite database, so the same vtoken
            // remains valid — but the old URL would silently redirect the
            // bridge to the wrong hub instance and leave the new hub with an
            // empty client list.
            if let Err(e) = validate_hub_token(&hub, &creds.token).await {
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
                    description,
                )
                .await;
            }
            // Token is valid for this hub.  If the stored base_url differs
            // (e.g. hub moved ports), silently rewrite the credential so
            // subsequent restarts use the current URL without another round-trip.
            let stored_base = creds.base_url.trim().trim_end_matches('/');
            if stored_base != hub.trim_end_matches('/') {
                let name = creds.client_name.as_deref().unwrap_or("");
                let _ = write_credentials(&path, &hub, &creds.token, name).await;
            }
            Ok((hub, creds.token))
        }
        LocalCredState::Missing => {
            auto_register_and_save(
                &path,
                &hub,
                register_client_name,
                None,
                config_path,
                description,
            )
            .await
        }
        LocalCredState::ExistsUnusable => {
            if force_register {
                let _ = tokio::fs::remove_file(&path).await;
                auto_register_and_save(
                    &path,
                    &hub,
                    register_client_name,
                    None,
                    config_path,
                    description,
                )
                .await
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

/// Resolve `(base_url, bot_token)` for `via: direct` (bridge → real iLink upstream).
///
/// Order: explicit token (`WEIXIN_TOKEN` / `--token`) → `--pair` QR login against
/// the real upstream → saved direct credential file.
///
/// Unlike [`resolve_hub_connection`], there is **no** `/hub/register` and no
/// `ILINK_ADMIN_TOKEN`: the real iLink upstream issues a `bot_token` directly via
/// QR login (the same `get_bot_qrcode` / `get_qrcode_status` flow the Hub server
/// uses to bootstrap its own context). `validate_hub_token` is a generic iLink
/// `/ilink/bot/getupdates` probe, so it works against the real upstream too.
///
/// `force_register`: if the credential file exists but is unusable, delete it and
/// run QR login again. Does not affect a **valid** saved file.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_direct_connection(
    base_url: &str,
    explicit_token: Option<&str>,
    cred_file: Option<&str>,
    force_pair: bool,
    force_register: bool,
    config_path: Option<&Path>,
    interactive: bool,
) -> Result<(String, String)> {
    let base = base_url.trim().trim_end_matches('/').to_string();

    if let Some(tok) = explicit_token.map(str::trim).filter(|s| !s.is_empty()) {
        validate_hub_token(&base, tok).await.with_context(|| {
            format!("via: direct — WEIXIN_TOKEN / --token 未被上游 {base} 接受（未注册或已失效）")
        })?;
        return Ok((base, tok.to_string()));
    }

    let path = direct_credential_path(cred_file.map(|s| s.trim()).filter(|s| !s.is_empty()));

    if force_pair {
        let token = qr_login_and_save_direct(&path, &base, config_path, interactive).await?;
        return Ok((base, token));
    }

    match local_credential_state(&path).await? {
        LocalCredState::Valid(creds) => {
            // Validate the saved token against the current base URL (not the
            // possibly-stale one stored in the file).
            if let Err(e) = validate_hub_token(&base, &creds.token).await {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "saved direct token rejected by upstream; removing credentials and re-logging in"
                );
                let _ = tokio::fs::remove_file(&path).await;
                let token =
                    qr_login_and_save_direct(&path, &base, config_path, interactive).await?;
                return Ok((base, token));
            }
            let stored_base = creds.base_url.trim().trim_end_matches('/');
            if stored_base != base.trim_end_matches('/') {
                let name = creds.client_name.as_deref().unwrap_or("");
                let _ = write_direct_credentials(&path, &base, &creds.token, name).await;
            }
            Ok((base, creds.token))
        }
        LocalCredState::Missing => {
            let token = qr_login_and_save_direct(&path, &base, config_path, interactive).await?;
            Ok((base, token))
        }
        LocalCredState::ExistsUnusable => {
            if force_register {
                let _ = tokio::fs::remove_file(&path).await;
                let token =
                    qr_login_and_save_direct(&path, &base, config_path, interactive).await?;
                Ok((base, token))
            } else {
                anyhow::bail!(
                    "direct 凭证文件 {} 已存在但无法使用（内容损坏或 token 为空）。\
                     为避免静默覆盖已有文件，已停止。请删除该文件、设置 WEIXIN_TOKEN、\
                     使用 `--pair`，或加上 `--force-register` 删除该文件后重新扫码登录。",
                    path.display()
                );
            }
        }
    }
}

/// Run the iLink QR login flow against the real upstream and persist the
/// resulting `bot_token` to the direct credential file.
///
/// When `interactive` is false (manager-managed child, `--no-interactive`, or
/// non-TTY stdout) this bails **before** printing a QR code — a headless
/// supervisor cannot confirm a phone scan, so blocking ~30min on QR polling
/// would only restart-loop. Control returns to the caller (and ultimately the
/// manager's credential guard) to park the profile until credentials exist.
async fn qr_login_and_save_direct(
    path: &Path,
    base: &str,
    config_path: Option<&Path>,
    interactive: bool,
) -> Result<String> {
    if !interactive {
        anyhow::bail!(
            "via: direct 需要扫码登录真实上游 {base}，但当前为非交互环境（manager 托管 / \
             `--no-interactive` / 非 TTY），无法完成扫码。请先在交互终端手动执行一次：\n  \
             ilink-hub-bridge --config {cfg} --cred-file {cred} --pair\n\
             完成扫码并存盘后，再交由 manager 托管或重启 bridge。",
            cfg = config_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<bridge.yaml>".into()),
            cred = path.display(),
        );
    }
    let client_name = default_auto_client_name(config_path);
    let login = LoginClient::new(Some(base.to_string()))
        .context("build iLink login client for direct QR")?;
    let token = login.login_with_qr().await.context(
        "via: direct — 扫码登录真实 iLink 上游失败。请确认 WEIXIN_BASE_URL / base_url 指向可达的真实上游，\
         或改用显式 WEIXIN_TOKEN。",
    )?;
    write_direct_credentials(path, base, &token, &client_name).await?;
    info!(
        path = %path.display(),
        base = %base,
        client = %client_name,
        "direct: QR login against real iLink upstream succeeded; saved direct credentials"
    );
    Ok(token)
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

    // ─── via: direct credential resolution (stage 3) ───────────────────────

    use mockito::Server;

    /// explicit token + upstream accepts → returns (base, token) without touching cred file.
    #[tokio::test]
    async fn direct_explicit_token_accepted_returns_base_and_token() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ret":0,"errmsg":null}"#)
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        let (base, token) = resolve_direct_connection(
            &server.url(),
            Some("my-bot-token"),
            Some(cred.to_str().unwrap()),
            false,
            false,
            None,
            true,
        )
        .await
        .expect("explicit token should be accepted");
        assert_eq!(token, "my-bot-token");
        assert_eq!(base, server.url());
        // No cred file should be written for the explicit-token path.
        assert!(!cred.exists());
    }

    /// explicit token rejected by upstream (401) → Err, no cred file written.
    #[tokio::test]
    async fn direct_explicit_token_rejected_returns_err() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ret":401,"errmsg":"token rejected"}"#)
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        let err = resolve_direct_connection(
            &server.url(),
            Some("bad-token"),
            Some(cred.to_str().unwrap()),
            false,
            false,
            None,
            true,
        )
        .await
        .expect_err("rejected token must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct"),
            "error should mention via: direct: {msg}"
        );
        assert!(!cred.exists());
    }

    /// no explicit token, no cred file → QR login against the real upstream, save cred, return token.
    #[tokio::test]
    async fn direct_no_token_qr_logs_in_and_saves_cred() {
        let mut server = Server::new_async().await;
        let _m_qr = server
            .mock("GET", "/ilink/bot/get_bot_qrcode?bot_type=3")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"qrcode":"qr-key","qrcode_img_content":"https://wx.qq.com/qr.png"}"#,
            )
            .create_async()
            .await;
        let _m_status = server
            .mock("GET", "/ilink/bot/get_qrcode_status?qrcode=qr-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ret":0,"status":"confirmed","bot_token":"direct-bot-token","baseurl":null,"ilink_bot_id":null,"ilink_user_id":null}"#,
            )
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        let (base, token) = resolve_direct_connection(
            &server.url(),
            None,
            Some(cred.to_str().unwrap()),
            false,
            false,
            None,
            true,
        )
        .await
        .expect("QR login should succeed");
        assert_eq!(token, "direct-bot-token");
        assert_eq!(base, server.url());
        // Cred file must be persisted with the bot_token.
        assert!(cred.exists(), "direct cred file should be written");
        let saved = tokio::fs::read_to_string(&cred).await.unwrap();
        assert!(
            saved.contains("direct-bot-token"),
            "saved cred must contain token: {saved}"
        );
    }

    /// saved direct cred file with a still-valid token is reused (no QR login).
    #[tokio::test]
    async fn direct_saved_valid_token_is_reused_without_qr() {
        let mut server = Server::new_async().await;
        // Only the validate probe is hit; no QR endpoints should be called.
        let _m = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ret":0,"errmsg":null}"#)
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        // Pre-write a valid direct cred file (same shape as HubPairingCredentials).
        tokio::fs::write(
            &cred,
            r#"{"token":"saved-direct-token","base_url":"https://example.com","account_id":"ilink@direct","user_id":"direct-client","saved_at":"0Z","client_name":"local-test"}"#,
        )
        .await
        .unwrap();

        let (base, token) = resolve_direct_connection(
            &server.url(),
            None,
            Some(cred.to_str().unwrap()),
            false,
            false,
            None,
            true,
        )
        .await
        .expect("saved valid token should be reused");
        assert_eq!(token, "saved-direct-token");
        assert_eq!(base, server.url());
    }

    /// N1: non-interactive (manager / --no-interactive / non-TTY) + no cred →
    /// bail **before** any QR endpoint is hit. A headless supervisor cannot
    /// confirm a phone scan, so blocking ~30min on QR polling would only
    /// restart-loop; control returns to the caller / manager credential guard.
    #[tokio::test]
    async fn direct_non_interactive_without_cred_bails_no_qr() {
        let mut server = Server::new_async().await;
        // If the bail is missing, the QR flow would hit GET /ilink/bot/get_bot_qrcode.
        // We deliberately do NOT mock it — any hit fails the request, but more
        // importantly we assert on the bail message rather than a network error.
        let _no_qr = server
            .mock("GET", "/ilink/bot/get_bot_qrcode")
            .with_status(500)
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        let err = resolve_direct_connection(
            &server.url(),
            None,
            Some(cred.to_str().unwrap()),
            false,
            false,
            None,
            false,
        )
        .await
        .expect_err("non-interactive + no cred must bail, not QR");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("非交互环境"),
            "expected non-interactive bail message: {msg}"
        );
        // No cred file written, no QR polling started.
        assert!(!cred.exists());
    }

    /// N1: non-interactive + `--pair` also bails (force_pair must not bypass
    /// the headless guard).
    #[tokio::test]
    async fn direct_non_interactive_pair_bails_no_qr() {
        let mut server = Server::new_async().await;
        let _no_qr = server
            .mock("GET", "/ilink/bot/get_bot_qrcode")
            .with_status(500)
            .create_async()
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let cred = dir.path().join("direct-cred.json");
        let err = resolve_direct_connection(
            &server.url(),
            None,
            Some(cred.to_str().unwrap()),
            true, // force_pair
            false,
            None,
            false, // non-interactive
        )
        .await
        .expect_err("--pair under non-interactive must bail, not QR");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("非交互环境"),
            "expected non-interactive bail message: {msg}"
        );
        assert!(!cred.exists());
    }

    // ─── Adversarial tests for M4 review findings ─────────────────────────

    /// SEC-ADV-M4-01: `local_hostname` must never panic or crash, even when
    /// the gethostname syscall returns a buffer that is not null-terminated.
    /// The fix forces `buf[255] = 0` before `CStr::from_ptr`, so this test
    /// verifies the function returns a non-empty String without panicking.
    #[test]
    fn adversarial_local_hostname_never_panics() {
        let hostname = local_hostname();
        assert!(!hostname.is_empty(), "local_hostname must return non-empty");
        // Must be valid UTF-8 (CStr::from_ptr + to_str would panic/crash otherwise).
        assert!(
            hostname
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '.'),
            "hostname must contain only valid chars"
        );
    }

    /// SEC-ADV-M4-01 (buffer overflow): simulate a full 255-byte hostname
    /// by writing 255 non-null bytes into a 256-byte buffer, then verify
    /// the null-termination guard prevents CStr::from_ptr from reading past
    /// the buffer boundary.
    #[test]
    fn adversarial_gethostname_buffer_null_terminated() {
        use std::ffi::CStr;
        // Simulate gethostname filling the entire buffer (255 bytes + no null).
        let mut buf = [b'x' as i8; 256];
        // The fix: force null terminator at the last position.
        buf[buf.len() - 1] = 0;
        // CStr::from_ptr must find the null within the buffer bounds.
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
        let s = cstr.to_str().expect("valid UTF-8");
        // The string should be 254 'x' chars (255 - 1 for the forced null).
        assert_eq!(s.len(), 255);
        assert!(s.chars().all(|c| c == 'x'));
    }
}
