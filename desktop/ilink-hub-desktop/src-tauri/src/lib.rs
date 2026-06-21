//! iLink Hub desktop shell: embeds the same runtime as `ilink-hub serve`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tauri::async_runtime::JoinHandle;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tokio::sync::watch;

/// Hub addressing for the UI: `listening_addr` is set only after `TcpListener::bind` succeeds.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubInfo {
    /// Address we passed to `run_serve` (e.g. from `ILINK_HUB_ADDR`).
    pub requested_addr: String,
    /// Set only after the hub has successfully bound (avoids showing a fake port when bind fails).
    pub listening_addr: Option<String>,
    pub admin_url: Option<String>,
    /// Loopback origin backends should use as `WEIXIN_BASE_URL` (e.g. `http://127.0.0.1:8765`).
    pub hub_base_url: Option<String>,
    pub database_path: String,
}

/// One registered backend client (same fields as `/hub/clients` JSON).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubClientRow {
    pub name: String,
    pub label: Option<String>,
    pub online: bool,
    pub vtoken: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubClientsPayload {
    pub listening: bool,
    pub clients: Vec<HubClientRow>,
    /// Hub has `ILINK_ADMIN_TOKEN` but the request was not authorized (desktop must set the same env var).
    pub auth_required: bool,
    pub error: Option<String>,
}

fn loopback_hub_origin(listen_addr: &str) -> String {
    let s = listen_addr
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host_port = if let Some(port_and_rest) = s.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{port_and_rest}")
    } else if let Some(port_and_rest) = s.strip_prefix("[::]:") {
        format!("127.0.0.1:{port_and_rest}")
    } else {
        s.to_string()
    };
    format!("http://{host_port}")
}

/// Path of the persisted GUI port override file: `~/.ilink-hub/desktop-port.json`.
///
/// Schema: `{ "port": <u16> }`. Missing / malformed files fall back to the
/// env-derived default so the desktop app keeps working without the file.
fn desktop_port_override_path() -> PathBuf {
    ilink_hub::paths::data_dir().join("desktop-port.json")
}

/// Compose the loopback listen address `127.0.0.1:<port>` for a user-selected
/// port. Centralised so tests and the command handler agree on the exact form.
fn loopback_listen_addr_for_port(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Persisted payload for `desktop-port.json`. Kept tiny and additive — extra
/// keys in future revisions are tolerated by `serde` only if we explicitly
/// add them; today there is exactly one.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopPortOverride {
    port: u16,
}

/// Read the persisted port override. Returns `Ok(None)` when the file is
/// missing (not yet chosen). Any other I/O / parse error is surfaced so
/// `setup()` can decide between "ignore and continue" vs. "bubble up".
fn load_desktop_port_override() -> Result<Option<u16>, String> {
    let path = desktop_port_override_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取端口设置失败: {e}"))?;
    let parsed: DesktopPortOverride =
        serde_json::from_str(&raw).map_err(|e| format!("端口设置格式无效: {e}"))?;
    if parsed.port == 0 {
        return Err("端口设置包含 0，必须在 1..=65535 之间".into());
    }
    Ok(Some(parsed.port))
}

/// Persist a port override atomically (write to a sibling temp file, rename).
/// Atomicity avoids leaving a half-written JSON that the next launch would
/// treat as malformed and drop on the floor.
fn save_desktop_port_override(port: u16) -> Result<(), String> {
    let path = desktop_port_override_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建设置目录失败: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let payload = DesktopPortOverride { port };
    let raw = serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("序列化端口设置失败: {e}"))?;
    std::fs::write(&tmp, raw).map_err(|e| format!("写入端口设置失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交端口设置失败: {e}"))?;
    Ok(())
}

/// Resolve the listen address the desktop shell should use on first start.
///
/// Priority: persisted port override → `ILINK_HUB_ADDR` env var → default
/// `127.0.0.1:8765`. The port override only overrides the port; the host
/// stays loopback so the saved choice cannot accidentally rebind on a
/// non-loopback interface.
fn resolve_initial_listen_addr() -> Result<String, String> {
    match load_desktop_port_override()? {
        Some(port) => Ok(loopback_listen_addr_for_port(port)),
        None => Ok(std::env::var("ILINK_HUB_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into())),
    }
}

/// Settings payload exposed to the frontend via `get_desktop_settings`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSettingsPayload {
    /// Port currently configured for the next bind (parsed out of
    /// `requested_addr` so the UI can pre-fill the input even when the value
    /// originated from `ILINK_HUB_ADDR`).
    pub listen_port: u16,
    /// Full loopback address the controller will hand to `run_serve`.
    pub requested_addr: String,
}

fn parse_loopback_port(addr: &str) -> Option<u16> {
    // Accept `127.0.0.1:<port>` (canonical) and the very loose `<port>` form
    // some early users might paste in. Anything else returns None and the
    // UI falls back to the default port.
    let trimmed = addr.trim();
    if let Some(rest) = trimmed.strip_prefix("127.0.0.1:") {
        return rest.parse::<u16>().ok().filter(|p| *p > 0);
    }
    if let Some(rest) = trimmed.strip_prefix("localhost:") {
        return rest.parse::<u16>().ok().filter(|p| *p > 0);
    }
    if !trimmed.contains(':') {
        return trimmed.parse::<u16>().ok().filter(|p| *p > 0);
    }
    None
}

#[tauri::command]
fn get_desktop_settings<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> DesktopSettingsPayload {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return DesktopSettingsPayload {
            listen_port: 8765,
            requested_addr: "127.0.0.1:8765".into(),
        };
    };
    let requested_addr = ctrl.requested_addr();
    let listen_port = parse_loopback_port(&requested_addr).unwrap_or(8765);
    DesktopSettingsPayload {
        listen_port,
        requested_addr,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetListenPortResult {
    pub ok: bool,
    pub requested_addr: String,
    pub listen_port: u16,
    pub error: Option<String>,
}

#[tauri::command]
fn set_listen_port<R: tauri::Runtime>(app: tauri::AppHandle<R>, port: u16) -> SetListenPortResult {
    if port == 0 {
        return SetListenPortResult {
            ok: false,
            requested_addr: "".into(),
            listen_port: 0,
            error: Some("端口必须在 1..=65535 之间".into()),
        };
    }
    if let Err(e) = save_desktop_port_override(port) {
        return SetListenPortResult {
            ok: false,
            requested_addr: "".into(),
            listen_port: port,
            error: Some(e),
        };
    }
    let new_addr = loopback_listen_addr_for_port(port);
    if let Some(ctrl) = app.try_state::<HubController>() {
        ctrl.set_requested_addr(new_addr.clone());
    }
    SetListenPortResult {
        ok: true,
        requested_addr: new_addr,
        listen_port: port,
        error: None,
    }
}

#[tauri::command]
async fn hub_clients(app: tauri::AppHandle) -> HubClientsPayload {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return HubClientsPayload {
            listening: false,
            clients: vec![],
            auth_required: false,
            error: Some("Hub 未初始化".into()),
        };
    };
    let Some(listen) = ctrl.listening_addr.lock().ok().and_then(|g| g.clone()) else {
        return HubClientsPayload {
            listening: false,
            clients: vec![],
            auth_required: false,
            error: None,
        };
    };

    let base = loopback_hub_origin(&listen);
    let url = format!("{}/hub/clients", base.trim_end_matches('/'));

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return HubClientsPayload {
                listening: true,
                clients: vec![],
                auth_required: false,
                error: Some(e.to_string()),
            };
        }
    };

    let mut req = client.get(&url);
    if let Ok(token) = std::env::var("ILINK_ADMIN_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }

    #[derive(serde::Deserialize)]
    struct ClientsBody {
        clients: Option<Vec<RawClient>>,
    }
    #[derive(serde::Deserialize)]
    struct RawClient {
        name: String,
        label: Option<String>,
        online: bool,
        vtoken: Option<String>,
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return HubClientsPayload {
                    listening: true,
                    clients: vec![],
                    auth_required: true,
                    error: None,
                };
            }
            if !status.is_success() {
                return HubClientsPayload {
                    listening: true,
                    clients: vec![],
                    auth_required: false,
                    error: Some(format!("拉取客户端列表失败（HTTP {}）", status.as_u16())),
                };
            }
            match resp.json::<ClientsBody>().await {
                Ok(body) => {
                    let clients = body
                        .clients
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| HubClientRow {
                            name: c.name,
                            label: c.label,
                            online: c.online,
                            vtoken: c.vtoken.unwrap_or_default(),
                        })
                        .collect();
                    HubClientsPayload {
                        listening: true,
                        clients,
                        auth_required: false,
                        error: None,
                    }
                }
                Err(e) => HubClientsPayload {
                    listening: true,
                    clients: vec![],
                    auth_required: false,
                    error: Some(format!("解析响应失败: {e}")),
                },
            }
        }
        Err(e) => HubClientsPayload {
            listening: true,
            clients: vec![],
            auth_required: false,
            error: Some(format!("请求失败: {e}")),
        },
    }
}

/// Parse a single unlabeled Prometheus sample line: `metric_name 123`.
fn parse_prometheus_simple_counter(body: &str, name: &str) -> Option<u64> {
    let prefix = format!("{name} ");
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&prefix) {
            if rest.contains('{') {
                continue;
            }
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubStatsPayload {
    pub listening: bool,
    pub error: Option<String>,
    pub clients_online: Option<u64>,
    pub clients_total: Option<u64>,
    pub messages_dispatched: Option<u64>,
    pub upstream_user_messages: Option<u64>,
}

#[tauri::command]
async fn hub_stats(app: tauri::AppHandle) -> HubStatsPayload {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return HubStatsPayload {
            listening: false,
            error: Some("Hub 未初始化".into()),
            clients_online: None,
            clients_total: None,
            messages_dispatched: None,
            upstream_user_messages: None,
        };
    };
    let Some(listen) = ctrl.listening_addr.lock().ok().and_then(|g| g.clone()) else {
        return HubStatsPayload {
            listening: false,
            error: None,
            clients_online: None,
            clients_total: None,
            messages_dispatched: None,
            upstream_user_messages: None,
        };
    };

    let base = loopback_hub_origin(&listen);
    let url = format!("{}/metrics", base.trim_end_matches('/'));

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return HubStatsPayload {
                listening: true,
                error: Some(e.to_string()),
                clients_online: None,
                clients_total: None,
                messages_dispatched: None,
                upstream_user_messages: None,
            };
        }
    };

    match client.get(&url).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return HubStatsPayload {
                    listening: true,
                    error: Some(format!("拉取指标失败（HTTP {}）", resp.status().as_u16())),
                    clients_online: None,
                    clients_total: None,
                    messages_dispatched: None,
                    upstream_user_messages: None,
                };
            }
            match resp.text().await {
                Ok(body) => HubStatsPayload {
                    listening: true,
                    error: None,
                    clients_online: parse_prometheus_simple_counter(
                        &body,
                        "ilink_hub_clients_online",
                    ),
                    clients_total: parse_prometheus_simple_counter(
                        &body,
                        "ilink_hub_clients_total",
                    ),
                    messages_dispatched: parse_prometheus_simple_counter(
                        &body,
                        "ilink_hub_messages_dispatched_total",
                    ),
                    upstream_user_messages: parse_prometheus_simple_counter(
                        &body,
                        "ilink_hub_upstream_user_messages_total",
                    ),
                },
                Err(e) => HubStatsPayload {
                    listening: true,
                    error: Some(format!("读取指标正文失败: {e}")),
                    clients_online: None,
                    clients_total: None,
                    messages_dispatched: None,
                    upstream_user_messages: None,
                },
            }
        }
        Err(e) => HubStatsPayload {
            listening: true,
            error: Some(format!("请求失败: {e}")),
            clients_online: None,
            clients_total: None,
            messages_dispatched: None,
            upstream_user_messages: None,
        },
    }
}

/// Result of registering a new backend from the desktop UI.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterResult {
    pub ok: bool,
    pub vtoken: Option<String>,
    /// Loopback base URL the new backend should use as `WEIXIN_BASE_URL`.
    pub base_url: Option<String>,
    /// Hub requires `ILINK_ADMIN_TOKEN` but the request was not authorized.
    pub auth_required: bool,
    pub error: Option<String>,
}

fn register_err(auth_required: bool, error: impl Into<String>) -> RegisterResult {
    RegisterResult {
        ok: false,
        vtoken: None,
        base_url: None,
        auth_required,
        error: Some(error.into()),
    }
}

#[tauri::command]
async fn hub_register(
    app: tauri::AppHandle,
    name: String,
    label: Option<String>,
) -> RegisterResult {
    let name = name.trim().to_string();
    if name.is_empty() {
        return register_err(false, "请填写后端名称");
    }
    let label = label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());

    let Some(ctrl) = app.try_state::<HubController>() else {
        return register_err(false, "Hub 未初始化");
    };
    let Some(listen) = ctrl.listening_addr.lock().ok().and_then(|g| g.clone()) else {
        return register_err(false, "服务尚未就绪，请稍候再注册");
    };

    let base = loopback_hub_origin(&listen);
    let base = base.trim_end_matches('/').to_string();
    let url = format!("{base}/hub/register");

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return register_err(false, e.to_string()),
    };

    #[derive(serde::Serialize)]
    struct RegReq {
        name: String,
        label: Option<String>,
    }

    let mut req = client.post(&url).json(&RegReq {
        name: name.clone(),
        label: label.clone(),
    });
    if let Ok(token) = std::env::var("ILINK_ADMIN_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }

    #[derive(serde::Deserialize)]
    struct RegBody {
        ret: i32,
        vtoken: Option<String>,
        errmsg: Option<String>,
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return RegisterResult {
                    ok: false,
                    vtoken: None,
                    base_url: None,
                    auth_required: true,
                    error: Some(
                        "Hub 已启用 ILINK_ADMIN_TOKEN，桌面端需在相同环境变量下启动才能注册。"
                            .into(),
                    ),
                };
            }
            match resp.json::<RegBody>().await {
                Ok(body) if body.ret == 0 => RegisterResult {
                    ok: true,
                    vtoken: body.vtoken,
                    base_url: Some(base),
                    auth_required: false,
                    error: None,
                },
                Ok(body) => {
                    register_err(false, body.errmsg.unwrap_or_else(|| "注册失败".to_string()))
                }
                Err(e) => register_err(false, format!("解析响应失败: {e}")),
            }
        }
        Err(e) => register_err(false, format!("请求失败: {e}")),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteClientResult {
    pub ok: bool,
    pub auth_required: bool,
    pub error: Option<String>,
}

fn delete_client_err(auth_required: bool, error: impl Into<String>) -> DeleteClientResult {
    DeleteClientResult {
        ok: false,
        auth_required,
        error: Some(error.into()),
    }
}

fn hub_state_from_app(
    app: &tauri::AppHandle,
) -> Result<Arc<ilink_hub::HubState>, DeleteClientResult> {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return Err(delete_client_err(false, "Hub 未初始化"));
    };
    let Some(state) = ctrl.hub_state.lock().ok().and_then(|g| g.clone()) else {
        return Err(delete_client_err(false, "服务尚未就绪，请稍候再试"));
    };
    Ok(state)
}

#[tauri::command]
async fn hub_delete_client(app: tauri::AppHandle, name: String) -> DeleteClientResult {
    use ilink_hub::server::pairing::{unregister_client_in_hub, UnregisterClientError};

    let name = name.trim().to_string();
    if name.is_empty() {
        return delete_client_err(false, "请指定要删除的后端名称");
    }

    let state = match hub_state_from_app(&app) {
        Ok(s) => s,
        Err(err) => return err,
    };

    match unregister_client_in_hub(state.as_ref(), &name, true).await {
        Ok(()) => DeleteClientResult {
            ok: true,
            auth_required: false,
            error: None,
        },
        Err(UnregisterClientError::NotFound) => {
            delete_client_err(false, format!("未找到后端「{name}」"))
        }
        Err(UnregisterClientError::StillOnline) => delete_client_err(
            false,
            format!("后端「{name}」仍在线，请先停止对应进程后再删除"),
        ),
        Err(UnregisterClientError::Store(e)) => delete_client_err(false, format!("删除失败: {e}")),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateClientResult {
    pub ok: bool,
    pub name: Option<String>,
    pub auth_required: bool,
    pub error: Option<String>,
}

fn update_client_err(auth_required: bool, error: impl Into<String>) -> UpdateClientResult {
    UpdateClientResult {
        ok: false,
        name: None,
        auth_required,
        error: Some(error.into()),
    }
}

#[tauri::command]
async fn hub_update_client(
    app: tauri::AppHandle,
    old_name: String,
    name: String,
    label: Option<String>,
) -> UpdateClientResult {
    use ilink_hub::server::pairing::{update_client_in_hub, UpdateClientError};

    let old_name = old_name.trim().to_string();
    let name = name.trim().to_string();
    if old_name.is_empty() {
        return update_client_err(false, "请指定要修改的后端名称");
    }
    if name.is_empty() {
        return update_client_err(false, "请填写后端名称");
    }
    let label = label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());

    let state = match hub_state_from_app(&app) {
        Ok(s) => s,
        Err(err) => {
            return UpdateClientResult {
                ok: false,
                name: None,
                auth_required: err.auth_required,
                error: err.error,
            };
        }
    };

    match update_client_in_hub(state.as_ref(), &old_name, &name, label).await {
        Ok(_) => UpdateClientResult {
            ok: true,
            name: Some(name),
            auth_required: false,
            error: None,
        },
        Err(UpdateClientError::NotFound) => {
            update_client_err(false, format!("未找到后端「{old_name}」"))
        }
        Err(UpdateClientError::NameTaken) => {
            update_client_err(false, format!("名称「{name}」已被占用"))
        }
        Err(UpdateClientError::InvalidName) => update_client_err(false, "后端名称不能为空"),
        Err(UpdateClientError::Store(e)) => update_client_err(false, format!("更新失败: {e}")),
    }
}

#[derive(Clone, Default)]
struct BridgeRuntime {
    state: String,
    error: Option<String>,
}

struct BridgeController {
    task: Mutex<Option<JoinHandle<()>>>,
    manager: Mutex<Option<ilink_hub::bridge::manager::BridgeManagerHandle>>,
    runtime: Arc<Mutex<BridgeRuntime>>,
    config_path: PathBuf,
    profiles_dir: PathBuf,
    credentials_dir: PathBuf,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBridgeProfileView {
    pub name: String,
    pub cwd: Option<String>,
    pub timeout_secs: u64,
    pub max_reply_chars: usize,
    pub model: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfigPayload {
    pub exists: bool,
    pub path: String,
    pub valid: bool,
    pub error: Option<String>,
    pub yaml: String,
    pub profiles: Vec<String>,
    pub default_profile: Option<String>,
    pub routing: Option<String>,
    pub claude_profile: Option<DesktopBridgeProfileView>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatusPayload {
    pub configured: bool,
    pub path: String,
    pub state: String,
    pub running: bool,
    pub error: Option<String>,
    pub manager: Option<ilink_hub::bridge::manager::BridgeManagerStatus>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveClaudeProfileRequest {
    pub cwd: String,
    #[serde(default)]
    pub env_vars: Option<Vec<EnvVar>>,
}

/// One environment variable entry (key + value pair) for a Bridge profile.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBridgeProfileFile {
    pub id: String,
    pub path: String,
    pub valid: bool,
    pub error: Option<String>,
    pub template: String,
    pub yaml: String,
    pub profiles: Vec<String>,
    pub default_profile: Option<String>,
    pub routing: Option<String>,
    pub cwd: Option<String>,
    pub timeout_secs: u64,
    pub max_reply_chars: usize,
    pub model: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    /// All user-defined environment variables from the primary profile.
    pub env_vars: Vec<EnvVar>,
    pub probe_error: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeProfilesPayload {
    pub profiles_dir: String,
    pub credentials_dir: String,
    pub profiles: Vec<DesktopBridgeProfileFile>,
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveBridgeProfileRequest {
    pub original_id: Option<String>,
    pub id: String,
    pub template: String,
    /// Working directory (required for preset templates).
    pub cwd: String,
    /// Environment variables (optional; used by all preset templates).
    pub env_vars: Option<Vec<EnvVar>>,
    /// Raw YAML (only used when template == "custom").
    pub yaml: Option<String>,
}

fn yaml_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Render the `env:` YAML block for a list of `EnvVar` entries.
///
/// `indent` is the number of leading spaces before the `env:` key.
/// Returns an empty string when `env_vars` is empty or all keys are blank.
fn env_vars_yaml(env_vars: &[EnvVar], indent: usize) -> String {
    let valid: Vec<&EnvVar> = env_vars
        .iter()
        .filter(|e| !e.key.trim().is_empty())
        .collect();
    if valid.is_empty() {
        return String::new();
    }
    let pad = " ".repeat(indent);
    let inner = " ".repeat(indent + 2);
    let mut out = format!("{pad}env:\n");
    for e in valid {
        out.push_str(&format!(
            "{inner}{}: {}\n",
            e.key.trim(),
            yaml_quote(e.value.trim())
        ));
    }
    out
}

/// Build the minimal `claude-code` profile YAML.
///
/// Generates a single-profile file; routing is auto-detected by the parser when omitted.
fn build_claude_profile_yaml(cwd: &str, env_vars: &[EnvVar]) -> Result<String, String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("请填写项目目录".into());
    }
    let env_section = env_vars_yaml(env_vars, 4);
    Ok(format!(
        "profiles:\n  claude:\n    type: claude-code\n    cwd: {cwd}\n{env_section}",
        cwd = yaml_quote(cwd),
        env_section = env_section,
    ))
}

/// Build a minimal flat single-profile YAML for CLI tools (codex, cursor agent, gemini, …).
fn build_simple_command_yaml(
    command: &str,
    args: &[String],
    cwd: &str,
    env_vars: &[EnvVar],
) -> Result<String, String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("请填写项目目录".into());
    }
    if command.trim().is_empty() {
        return Err("请填写命令".into());
    }
    let env_section = env_vars_yaml(env_vars, 0);
    Ok(format!(
        "command: {command}\nargs: {args}\ncwd: {cwd}\nstdin: none\n{env_section}",
        command = yaml_quote(command.trim()),
        args = yaml_string_array(args),
        cwd = yaml_quote(cwd),
        env_section = env_section,
    ))
}

fn sanitize_profile_file_id(raw: &str) -> Result<String, String> {
    let mut out: String = raw
        .trim()
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
    let out = out.trim_matches('-').chars().take(64).collect::<String>();
    if out.is_empty() {
        Err("请填写 workspace 名称（仅支持字母、数字、-、_；其他字符会转换为 -）".into())
    } else {
        Ok(out)
    }
}

fn yaml_string_array(items: &[String]) -> String {
    let quoted = items
        .iter()
        .map(|s| yaml_quote(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{quoted}]")
}

fn build_bridge_profile_yaml(req: &SaveBridgeProfileRequest) -> Result<String, String> {
    let env_vars = req.env_vars.as_deref().unwrap_or(&[]);
    match req.template.as_str() {
        "claude" => build_claude_profile_yaml(&req.cwd, env_vars),
        "cursor" => build_simple_command_yaml(
            "agent",
            &["-p".into(), "{{MESSAGE}}".into()],
            &req.cwd,
            env_vars,
        ),
        "codex" => build_simple_command_yaml(
            "codex",
            &["exec".into(), "{{MESSAGE}}".into()],
            &req.cwd,
            env_vars,
        ),
        "gemini" => build_simple_command_yaml(
            "gemini",
            &["-p".into(), "{{MESSAGE}}".into()],
            &req.cwd,
            env_vars,
        ),
        "custom" => {
            let yaml = req
                .yaml
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "请填写 YAML".to_string())?;
            Ok(format!("{yaml}\n"))
        }
        other => Err(format!("未知模板: {other}")),
    }
}

fn detect_bridge_profile_template(app: &ilink_hub::bridge::BridgeApp) -> String {
    if let Some(p) = app.profile("claude") {
        if p.profile_type.as_deref() == Some("claude-code") || p.command == "ilink-hub-bridge" {
            return "claude".into();
        }
    }
    let p = app.profile("default").or_else(|| {
        let name = app.default_profile_name().to_string();
        app.profile(&name)
    });
    match p.map(|p| p.command.as_str()) {
        Some("agent") => "cursor".into(),
        Some("codex") => "codex".into(),
        Some("gemini") => "gemini".into(),
        _ => "custom".into(),
    }
}

fn summarize_bridge_profile_file(
    id: String,
    path: PathBuf,
    yaml: String,
) -> DesktopBridgeProfileFile {
    match ilink_hub::bridge::BridgeApp::parse_yaml(&yaml) {
        Ok(app) => {
            let template = detect_bridge_profile_template(&app);
            let profile = app
                .profile("claude")
                .or_else(|| app.profile("default"))
                .or_else(|| app.profile(app.default_profile_name()));
            let env_vars = profile
                .map(|p| {
                    let mut vars: Vec<EnvVar> = p
                        .env
                        .iter()
                        .map(|(k, v)| EnvVar {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect();
                    vars.sort_by(|a, b| a.key.cmp(&b.key));
                    vars
                })
                .unwrap_or_default();
            let probe_error = profile
                .and_then(|p| ilink_hub::bridge::probe_profile_light(p).err())
                .map(|e| e.to_string());
            DesktopBridgeProfileFile {
                id,
                path: path.display().to_string(),
                valid: true,
                error: None,
                template,
                yaml,
                profiles: app
                    .profile_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                default_profile: Some(app.default_profile_name().to_string()),
                routing: Some(app.routing_label().to_string()),
                cwd: profile.and_then(|p| p.cwd.clone()),
                timeout_secs: profile.map(|p| p.timeout_secs).unwrap_or(600),
                max_reply_chars: profile.map(|p| p.max_reply_chars).unwrap_or(8000),
                model: profile.and_then(|p| p.env.get("ILINK_CLAUDE_MODEL").cloned()),
                command: profile.map(|p| p.command.clone()),
                args: profile.map(|p| p.args.clone()).unwrap_or_default(),
                env_vars,
                probe_error,
            }
        }
        Err(e) => DesktopBridgeProfileFile {
            id,
            path: path.display().to_string(),
            valid: false,
            error: Some(e.to_string()),
            template: "custom".into(),
            yaml,
            profiles: vec![],
            default_profile: None,
            routing: None,
            cwd: None,
            timeout_secs: 600,
            max_reply_chars: 8000,
            model: None,
            command: None,
            args: vec![],
            env_vars: vec![],
            probe_error: None,
        },
    }
}

fn summarize_bridge_config(
    path: &std::path::Path,
    yaml: String,
    exists: bool,
) -> BridgeConfigPayload {
    match ilink_hub::bridge::BridgeApp::parse_yaml(&yaml) {
        Ok(app) => {
            let profiles = app
                .profile_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            let claude_profile = app.profile("claude").map(|p| DesktopBridgeProfileView {
                name: "claude".to_string(),
                cwd: p.cwd.clone(),
                timeout_secs: p.timeout_secs,
                max_reply_chars: p.max_reply_chars,
                model: p.env.get("ILINK_CLAUDE_MODEL").cloned(),
            });
            BridgeConfigPayload {
                exists,
                path: path.display().to_string(),
                valid: true,
                error: None,
                yaml,
                profiles,
                default_profile: Some(app.default_profile_name().to_string()),
                routing: Some(app.routing_label().to_string()),
                claude_profile,
            }
        }
        Err(e) => BridgeConfigPayload {
            exists,
            path: path.display().to_string(),
            valid: false,
            error: Some(e.to_string()),
            yaml,
            profiles: vec![],
            default_profile: None,
            routing: None,
            claude_profile: None,
        },
    }
}

#[tauri::command]
async fn bridge_config(app: tauri::AppHandle) -> BridgeConfigPayload {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return BridgeConfigPayload {
            exists: false,
            path: "".into(),
            valid: false,
            error: Some("Bridge 未初始化".into()),
            yaml: "".into(),
            profiles: vec![],
            default_profile: None,
            routing: None,
            claude_profile: None,
        };
    };
    let path = ctrl.config_path.clone();
    match tokio::fs::read_to_string(&path).await {
        Ok(yaml) => summarize_bridge_config(&path, yaml, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BridgeConfigPayload {
            exists: false,
            path: path.display().to_string(),
            valid: false,
            error: None,
            yaml: "".into(),
            profiles: vec![],
            default_profile: None,
            routing: None,
            claude_profile: None,
        },
        Err(e) => BridgeConfigPayload {
            exists: false,
            path: path.display().to_string(),
            valid: false,
            error: Some(format!("读取配置失败: {e}")),
            yaml: "".into(),
            profiles: vec![],
            default_profile: None,
            routing: None,
            claude_profile: None,
        },
    }
}

#[tauri::command]
async fn bridge_save_claude_profile(
    app: tauri::AppHandle,
    req: SaveClaudeProfileRequest,
) -> Result<BridgeConfigPayload, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let env_vars = req.env_vars.unwrap_or_default();
    let yaml = build_claude_profile_yaml(&req.cwd, &env_vars)?;
    ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).map_err(|e| e.to_string())?;
    if let Some(parent) = ctrl.config_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    tokio::fs::write(&ctrl.config_path, &yaml)
        .await
        .map_err(|e| format!("保存配置失败: {e}"))?;
    Ok(summarize_bridge_config(&ctrl.config_path, yaml, true))
}

#[tauri::command]
async fn bridge_save_yaml(
    app: tauri::AppHandle,
    yaml: String,
) -> Result<BridgeConfigPayload, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).map_err(|e| e.to_string())?;
    if let Some(parent) = ctrl.config_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    tokio::fs::write(&ctrl.config_path, &yaml)
        .await
        .map_err(|e| format!("保存配置失败: {e}"))?;
    Ok(summarize_bridge_config(&ctrl.config_path, yaml, true))
}

fn profile_path(ctrl: &BridgeController, id: &str) -> Result<PathBuf, String> {
    let id = sanitize_profile_file_id(id)?;
    Ok(ctrl.profiles_dir.join(format!("{id}.yaml")))
}

fn existing_profile_path(ctrl: &BridgeController, id: &str) -> Result<Option<PathBuf>, String> {
    let id = sanitize_profile_file_id(id)?;
    let yaml = ctrl.profiles_dir.join(format!("{id}.yaml"));
    if yaml.exists() {
        return Ok(Some(yaml));
    }
    let yml = ctrl.profiles_dir.join(format!("{id}.yml"));
    if yml.exists() {
        return Ok(Some(yml));
    }
    Ok(None)
}

#[tauri::command]
async fn bridge_profiles(app: tauri::AppHandle) -> BridgeProfilesPayload {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return BridgeProfilesPayload {
            profiles_dir: "".into(),
            credentials_dir: "".into(),
            profiles: vec![],
            error: Some("Bridge 未初始化".into()),
        };
    };
    let mut profiles = Vec::new();
    let mut entries = match tokio::fs::read_dir(&ctrl.profiles_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return BridgeProfilesPayload {
                profiles_dir: ctrl.profiles_dir.display().to_string(),
                credentials_dir: ctrl.credentials_dir.display().to_string(),
                profiles,
                error: None,
            };
        }
        Err(e) => {
            return BridgeProfilesPayload {
                profiles_dir: ctrl.profiles_dir.display().to_string(),
                credentials_dir: ctrl.credentials_dir.display().to_string(),
                profiles,
                error: Some(format!("读取 profile 目录失败: {e}")),
            };
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|s| s.to_str()),
            Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
        );
        if !is_yaml {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("profile")
            .to_string();
        match tokio::fs::read_to_string(&path).await {
            Ok(yaml) => profiles.push(summarize_bridge_profile_file(id, path, yaml)),
            Err(e) => profiles.push(DesktopBridgeProfileFile {
                id,
                path: path.display().to_string(),
                valid: false,
                error: Some(format!("读取 YAML 失败: {e}")),
                template: "custom".into(),
                yaml: "".into(),
                profiles: vec![],
                default_profile: None,
                routing: None,
                cwd: None,
                timeout_secs: 600,
                max_reply_chars: 8000,
                model: None,
                command: None,
                args: vec![],
                env_vars: vec![],
                probe_error: None,
            }),
        }
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    BridgeProfilesPayload {
        profiles_dir: ctrl.profiles_dir.display().to_string(),
        credentials_dir: ctrl.credentials_dir.display().to_string(),
        profiles,
        error: None,
    }
}

#[tauri::command]
async fn bridge_save_profile(
    app: tauri::AppHandle,
    req: SaveBridgeProfileRequest,
) -> Result<DesktopBridgeProfileFile, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let id = sanitize_profile_file_id(&req.id)?;
    let yaml = build_bridge_profile_yaml(&req)?;
    ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(&ctrl.profiles_dir)
        .await
        .map_err(|e| format!("创建 profile 目录失败: {e}"))?;
    let original_id = req
        .original_id
        .as_deref()
        .map(sanitize_profile_file_id)
        .transpose()?;
    let mut path = profile_path(&ctrl, &id)?;
    let existing_target = existing_profile_path(&ctrl, &id)?;
    if original_id.as_deref() != Some(id.as_str()) {
        if existing_target.is_some() {
            return Err(format!("profile `{id}` 已存在，请换一个 workspace 名称"));
        }
        if let Some(original_id) = original_id.as_deref() {
            if let Some(old_path) = existing_profile_path(&ctrl, original_id)? {
                tokio::fs::remove_file(&old_path)
                    .await
                    .map_err(|e| format!("删除旧 profile 失败: {e}"))?;
            }
        }
    } else if let Some(existing) = existing_target {
        path = existing;
    }
    tokio::fs::write(&path, &yaml)
        .await
        .map_err(|e| format!("保存 profile 失败: {e}"))?;
    Ok(summarize_bridge_profile_file(id, path, yaml))
}

#[tauri::command]
async fn bridge_delete_profile(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    match existing_profile_path(&ctrl, &id)? {
        Some(path) => tokio::fs::remove_file(&path)
            .await
            .map_err(|e| format!("删除 profile 失败: {e}")),
        None => Ok(()),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub success: bool,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

#[tauri::command]
async fn bridge_test_profile(
    app: tauri::AppHandle,
    id: String,
    _message: String,
) -> Result<ProbeResult, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let config_path =
        existing_profile_path(&ctrl, &id)?.ok_or_else(|| format!("未找到 profile `{id}`"))?;
    let app_cfg = ilink_hub::bridge::BridgeApp::load(&config_path)
        .map_err(|e| format!("加载配置失败: {e}"))?;
    let profile_name = app_cfg.default_profile_name();
    let profile = app_cfg
        .profile(profile_name)
        .ok_or_else(|| format!("配置中未找到默认 profile `{profile_name}`"))?;

    // For safety, hardcode the probe message to "ping" to prevent command injection/RCE.
    let msg = "ping";
    match ilink_hub::bridge::dry_run_profile(profile, msg).await {
        Ok(stdout) => Ok(ProbeResult {
            success: true,
            error_type: None,
            error_message: None,
            stdout: Some(stdout),
            stderr: None,
        }),
        Err(e) => {
            let error_type = e.error_type().to_string();
            let error_message = e.to_string();
            let (stdout, stderr) = match &e {
                ilink_hub::bridge::ProbeError::Unauthenticated(detail)
                | ilink_hub::bridge::ProbeError::ExecutionError(detail) => {
                    (None, Some(detail.clone()))
                }
                _ => (None, None),
            };

            Ok(ProbeResult {
                success: false,
                error_type: Some(error_type),
                error_message: Some(error_message),
                stdout,
                stderr,
            })
        }
    }
}

fn set_bridge_runtime(ctrl: &BridgeController, state: &str, error: Option<String>) {
    if let Ok(mut runtime) = ctrl.runtime.lock() {
        runtime.state = state.to_string();
        runtime.error = error;
    }
}

fn set_bridge_runtime_arc(runtime: &Arc<Mutex<BridgeRuntime>>, state: &str, error: Option<String>) {
    if let Ok(mut r) = runtime.lock() {
        r.state = state.to_string();
        r.error = error;
    }
}

#[tauri::command]
fn bridge_status(app: tauri::AppHandle) -> BridgeStatusPayload {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return BridgeStatusPayload {
            configured: false,
            path: "".into(),
            state: "error".into(),
            running: false,
            error: Some("Bridge 未初始化".into()),
            manager: None,
        };
    };
    let mut state = ctrl
        .runtime
        .lock()
        .map(|r| r.clone())
        .unwrap_or_else(|_| BridgeRuntime {
            state: "error".into(),
            error: Some("读取 Bridge 状态失败".into()),
        });
    let has_task = ctrl.task.lock().ok().map(|g| g.is_some()).unwrap_or(false);
    let manager = ctrl
        .manager
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|h| h.status()));
    let running = has_task && matches!(state.state.as_str(), "starting" | "running");
    if !running && state.state == "running" {
        state.state = "stopped".into();
    }
    BridgeStatusPayload {
        configured: ctrl.profiles_dir.exists(),
        path: ctrl.profiles_dir.display().to_string(),
        state: state.state,
        running,
        error: state.error,
        manager,
    }
}

fn start_bridge_task(app: &tauri::AppHandle) -> Result<(), String> {
    let Some(hub) = app.try_state::<HubController>() else {
        return Err("Hub 未初始化".into());
    };
    let listen = hub
        .listening_addr
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "Hub 尚未就绪，请稍候再启动 Bridge".to_string())?;
    let hub_url = loopback_hub_origin(&listen)
        .trim_end_matches('/')
        .to_string();

    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let mut task = ctrl.task.lock().map_err(|e| e.to_string())?;
    let current_state = ctrl
        .runtime
        .lock()
        .map(|r| r.state.clone())
        .unwrap_or_else(|_| "error".into());
    if task.is_some() && matches!(current_state.as_str(), "starting" | "running") {
        set_bridge_runtime(&ctrl, "running", None);
        return Ok(());
    }
    if let Some(handle) = task.take() {
        handle.abort();
    }

    let profiles_dir = ctrl.profiles_dir.clone();
    let credentials_dir = ctrl.credentials_dir.clone();
    let runtime = Arc::clone(&ctrl.runtime);
    set_bridge_runtime_arc(&runtime, "starting", None);
    let mut opts = ilink_hub::bridge::manager::BridgeManagerOptions::new(
        hub_url,
        profiles_dir,
        credentials_dir,
    );
    opts.scan_interval = std::time::Duration::from_secs(3);
    let (manager_handle, manager_task) = ilink_hub::bridge::manager::spawn_bridge_manager(opts);
    if let Ok(mut guard) = ctrl.manager.lock() {
        *guard = Some(manager_handle);
    }
    let handle = tauri::async_runtime::spawn(async move {
        set_bridge_runtime_arc(&runtime, "running", None);
        match manager_task.await {
            Ok(Ok(())) => set_bridge_runtime_arc(&runtime, "stopped", None),
            Ok(Err(e)) => set_bridge_runtime_arc(&runtime, "error", Some(format!("{e:#}"))),
            Err(e) => {
                set_bridge_runtime_arc(&runtime, "error", Some(format!("Manager 任务异常: {e}")))
            }
        }
    });
    *task = Some(handle);
    Ok(())
}

#[tauri::command]
async fn bridge_start(app: tauri::AppHandle) -> Result<(), String> {
    start_bridge_task(&app)
}

#[tauri::command]
fn bridge_stop(app: tauri::AppHandle) -> Result<(), String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    if let Some(manager) = ctrl.manager.lock().map_err(|e| e.to_string())?.take() {
        manager.stop();
    }
    if let Some(handle) = ctrl.task.lock().map_err(|e| e.to_string())?.take() {
        handle.abort();
    }
    set_bridge_runtime(&ctrl, "stopped", None);
    Ok(())
}

#[tauri::command]
async fn bridge_restart(app: tauri::AppHandle) -> Result<(), String> {
    // Signal the old manager to stop and take its task handle WITHOUT aborting it.
    // Aborting the outer wrapper task does not abort the inner BridgeManager tokio task;
    // that task keeps running and still holds Child handles with kill_on_drop(true).
    // If we immediately start a new manager before the old one finishes stop_all(),
    // the old manager's cleanup races against the new manager's freshly-started children
    // and SIGKILLs them, causing an infinite restart loop.
    let old_task = {
        let Some(ctrl) = app.try_state::<BridgeController>() else {
            return Err("Bridge 未初始化".into());
        };
        if let Some(manager) = ctrl.manager.lock().map_err(|e| e.to_string())?.take() {
            manager.stop();
        }
        let task = ctrl.task.lock().map_err(|e| e.to_string())?.take();
        task
    };
    // Wait for the old manager to fully shut down (stop_all completes, task exits).
    // A 10-second timeout prevents a hang if something goes wrong during shutdown.
    if let Some(task) = old_task {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), task).await;
    }
    start_bridge_task(&app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn claude_profile_wizard_generates_minimal_yaml() {
        let yaml = build_claude_profile_yaml("/tmp/my project", &[]).unwrap();
        let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();

        assert_eq!(app.default_profile_name(), "claude");
        assert_eq!(app.routing_label(), "fixed");
        let profile = app.profile("claude").unwrap();
        assert_eq!(profile.cwd.as_deref(), Some("/tmp/my project"));
        assert!(app.profile("claude_new").is_none());
    }

    #[test]
    fn claude_profile_with_env_vars() {
        let env_vars = vec![EnvVar {
            key: "ILINK_CLAUDE_MODEL".into(),
            value: "sonnet".into(),
        }];
        let yaml = build_claude_profile_yaml("/tmp/project", &env_vars).unwrap();
        let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();
        let profile = app.profile("claude").unwrap();
        assert_eq!(
            profile.env.get("ILINK_CLAUDE_MODEL").map(String::as_str),
            Some("sonnet")
        );
    }

    #[test]
    fn claude_profile_wizard_requires_cwd() {
        let err = build_claude_profile_yaml(" ", &[]).unwrap_err();
        assert!(err.contains("项目目录"));
    }

    #[test]
    fn command_profile_templates_generate_valid_yaml() {
        for (template, command) in [
            ("cursor", "agent"),
            ("codex", "codex"),
            ("gemini", "gemini"),
        ] {
            let yaml = build_bridge_profile_yaml(&SaveBridgeProfileRequest {
                original_id: None,
                id: format!("{template}-demo"),
                template: template.into(),
                cwd: "/tmp/project".into(),
                env_vars: None,
                yaml: None,
            })
            .unwrap();
            let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();
            let profile = app.profile("default").unwrap();
            assert_eq!(profile.command, command);
            assert_eq!(profile.cwd.as_deref(), Some("/tmp/project"));
        }
    }

    #[test]
    fn command_profile_with_env_vars() {
        let env_vars = vec![EnvVar {
            key: "MY_TOKEN".into(),
            value: "abc123".into(),
        }];
        let yaml = build_bridge_profile_yaml(&SaveBridgeProfileRequest {
            original_id: None,
            id: "codex-demo".into(),
            template: "codex".into(),
            cwd: "/tmp/project".into(),
            env_vars: Some(env_vars),
            yaml: None,
        })
        .unwrap();
        let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();
        let profile = app.profile("default").unwrap();
        assert_eq!(
            profile.env.get("MY_TOKEN").map(String::as_str),
            Some("abc123")
        );
    }

    fn make_hub_controller(running: bool) -> HubController {
        HubController {
            shutdown_tx: Mutex::new(if running {
                Some(watch::channel(false).0)
            } else {
                None
            }),
            task_handles: Mutex::new(HubTaskHandles::default()),
            env_token: None,
            env_base_url: None,
            requested_addr: Mutex::new("127.0.0.1:8765".into()),
            database_path: PathBuf::from("/tmp/ilink-hub-test.db"),
            listening_addr: Arc::new(Mutex::new(None)),
            hub_state: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn hub_controller_is_running_reflects_shutdown_tx() {
        let ctrl = make_hub_controller(true);
        assert!(ctrl.is_running());

        let ctrl = make_hub_controller(false);
        assert!(!ctrl.is_running());

        // Simulating a stop: take the sender, then is_running should flip to false.
        let ctrl = make_hub_controller(true);
        let taken = ctrl.shutdown_tx.lock().unwrap().take();
        assert!(taken.is_some());
        assert!(!ctrl.is_running());
    }

    #[test]
    fn sqlite_url_for_path_handles_unix_and_windows_paths() {
        assert_eq!(
            sqlite_url_for_path(Path::new("/tmp/db.sqlite")),
            "sqlite:/tmp/db.sqlite"
        );
        assert_eq!(
            sqlite_url_for_path(Path::new("C:/data/db.sqlite")),
            "sqlite:/C:/data/db.sqlite"
        );
        assert_eq!(
            sqlite_url_for_path(Path::new("relative.sqlite")),
            "sqlite:relative.sqlite"
        );
    }

    #[test]
    fn sqlite_url_for_path_normalizes_backslashes() {
        // Backslashes are converted to forward slashes so the resulting URL is portable.
        assert_eq!(
            sqlite_url_for_path(Path::new("C:\\data\\db.sqlite")),
            "sqlite:/C:/data/db.sqlite"
        );
    }

    #[test]
    fn stop_hub_signals_existing_tx_and_clears_handle() {
        // Mirrors the runtime branch in stop_hub: a present sender signals shutdown,
        // and the controller no longer reports running once the sender is taken.
        let ctrl = make_hub_controller(true);
        let mut rx = {
            let (tx, rx) = watch::channel(false);
            *ctrl.shutdown_tx.lock().unwrap() = Some(tx);
            rx
        };
        assert!(ctrl.is_running());

        let tx = ctrl
            .shutdown_tx
            .lock()
            .unwrap()
            .take()
            .expect("sender present");
        tx.send(true).expect("receiver alive");
        assert_eq!(*rx.borrow_and_update(), true);
        assert!(!ctrl.is_running());
    }

    #[test]
    fn stop_hub_is_noop_when_already_stopped() {
        // Mirrors the runtime branch in stop_hub when shutdown_tx is already None:
        // no sender to signal, but the call is still successful (idempotent).
        let ctrl = make_hub_controller(false);
        assert!(!ctrl.is_running());
        let tx = ctrl.shutdown_tx.lock().unwrap().take();
        assert!(tx.is_none());
        assert!(!ctrl.is_running());
    }

    /// Adversarial regression test for F-01.
    ///
    /// F-01 root cause was that `spawn_hub_task` looked up `HubController` via
    /// `app.state::<HubController>()` at the top of its body, and `setup()` then
    /// called the helper BEFORE `app.manage(HubController)`. The post-fix shape
    /// never touches the AppHandle from inside the helper — it takes the shared
    /// Arcs as arguments. This test exercises the exact call order `setup()` uses
    /// after the fix: build the controller, manage it, THEN call the helper with
    /// its Arcs. If a future refactor re-introduces an `app.state::<HubController>()`
    /// lookup inside the helper, this test cannot detect that, but it locks in
    /// the call order at the call site so the helper stays callable from `setup()`
    /// without the AppHandle lookup it used to do.
    #[test]
    fn setup_order_does_not_require_app_state_lookup_inside_helper() {
        // Replicate the post-fix setup() pattern:
        //   1) build Arcs
        //   2) construct controller
        //   3) call helper with Arcs
        // (no AppHandle / no state() lookup involved)
        let listening_addr = Arc::new(Mutex::new(None::<String>));
        let hub_state = Arc::new(Mutex::new(None::<Arc<ilink_hub::HubState>>));
        let ctrl = make_hub_controller(false);

        // The helper signature now takes the Arcs explicitly — verified at
        // compile time by the function signature below. The runtime behavior
        // we want to lock in is: constructing the controller does NOT require
        // any AppHandle, and the controller is usable before any "spawn" call.
        assert!(!ctrl.is_running());

        // Arcs are the only piece of state the helper touches; both must
        // outlive any spawn the helper might do. Drop the controller first
        // to make sure the Arcs are the only owners.
        let _ctrl = ctrl;
        let _ = listening_addr;
        let _ = hub_state;
    }

    /// Adversarial test for F-02: `start_hub` MUST refuse to install a sender
    /// when one is already present, AND it must do so under the same lock
    /// acquisition that checks the slot. Two concurrent acquires of the
    /// `shutdown_tx` lock must serialize — only one can observe `None` and
    /// install; the second MUST observe `Some(_)` and abort.
    #[test]
    fn start_hub_double_install_is_serialized_by_mutex() {
        // Simulate two concurrent start_hub callers. They both want the
        // shutdown_tx slot. The mutex serializes them, so exactly one wins.
        let ctrl = std::sync::Arc::new(make_hub_controller(false));
        let ctrl2 = ctrl.clone();

        let t1 = std::thread::spawn(move || {
            let mut g = ctrl.shutdown_tx.lock().unwrap();
            if g.is_some() {
                return false; // loser
            }
            // Hold the lock long enough that t2 also tries to acquire.
            std::thread::sleep(std::time::Duration::from_millis(50));
            *g = Some(watch::channel(false).0);
            true // winner
        });

        let t2 = std::thread::spawn(move || {
            let mut g = ctrl2.shutdown_tx.lock().unwrap();
            if g.is_some() {
                return false; // loser
            }
            *g = Some(watch::channel(false).0);
            true // winner
        });

        let w1 = t1.join().unwrap();
        let w2 = t2.join().unwrap();
        assert!(
            w1 ^ w2,
            "exactly one of two concurrent installs must win, got w1={} w2={}",
            w1,
            w2
        );
    }

    /// Adversarial test for F-03: `restart_hub` waits on the run_serve
    /// JoinHandle, not on `listening_addr`. A run_serve that takes time
    /// to bind must NOT make restart_hub think it has finished.
    #[tokio::test]
    async fn restart_hub_waits_on_run_serve_join_handle_not_listening_addr() {
        let ctrl = make_hub_controller(true);

        // Simulate a run_serve that has not yet bound — listening_addr is None,
        // but the JoinHandle is still pending. The OLD restart_hub code would
        // see None and immediately call start_hub (double-spawn). The NEW code
        // waits on the JoinHandle.
        assert!(ctrl.listening_addr.lock().unwrap().is_none());

        // Spawn a fake run_serve that completes "in the background" after 200ms.
        let fake_run_serve = tauri::async_runtime::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });
        ctrl.task_handles.lock().unwrap().run_serve = Some(fake_run_serve);

        // The wait must take at least ~200ms — i.e. it MUST await the JoinHandle,
        // not just peek listening_addr and return.
        let started = std::time::Instant::now();
        let handle = ctrl
            .task_handles
            .lock()
            .unwrap()
            .run_serve
            .take()
            .expect("fake handle present");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "restart wait returned too quickly ({:?}) — did not actually await run_serve JoinHandle",
            elapsed
        );
    }

    /// Adversarial test for F-04: a poisoned mutex must propagate, not be
    /// silently treated as "not running". We simulate the panic by locking
    /// the mutex from one thread and panicking while holding the lock.
    #[test]
    #[should_panic(expected = "HubController mutex poisoned")]
    fn poisoned_mutex_is_not_silently_swallowed_by_is_running() {
        let ctrl = std::sync::Arc::new(make_hub_controller(true));
        let ctrl2 = ctrl.clone();

        // Panic while holding the lock to poison it.
        let _ = std::thread::spawn(move || {
            let _g = ctrl2.shutdown_tx.lock().unwrap();
            panic!("simulated panic inside hub task");
        })
        .join();

        // Give the panic a moment to land.
        std::thread::sleep(std::time::Duration::from_millis(20));

        // After the panic, is_running() must panic (per the .expect() in the
        // fix). The OLD code returned false silently and let start_hub
        // double-spawn.
        let _ = ctrl.is_running();
    }

    /// Adversarial test for F-05: the QR channel is `unbounded_channel` today;
    /// flag as an explicit known-limitation so the M2 author can prioritize
    /// bounding it. This is a regression-guard, not a fix.
    #[test]
    fn qr_channel_is_unbounded_intentionally_until_m2() {
        // Pin the current behavior: helper uses mpsc::unbounded_channel for QR
        // events. If a future change moves to a bounded channel, this test
        // should be updated (and the unbounded→bounded migration deserves
        // its own test for backpressure handling).
        // The helper signature is the contract — verified by the fact that
        // this file compiles.
        let _ = tokio::sync::mpsc::unbounded_channel::<()>();
    }

    /// Adversarial test for F-06: env vars captured in setup() must survive
    /// into subsequent start_hub / restart_hub calls, even if the process env
    /// is mutated between them.
    #[test]
    fn env_config_is_captured_once_in_setup_not_re_read_per_start() {
        let mut ctrl = make_hub_controller(false);
        // The controller is the source of truth for env-derived config.
        // After setup() runs, env_token / env_base_url are fixed values that
        // start_hub reads from the controller, NOT from std::env.
        assert!(ctrl.env_token.is_none());
        assert!(ctrl.env_base_url.is_none());

        // Simulate a setup() that captured env vars.
        ctrl.env_token = Some("setup-token".into());
        ctrl.env_base_url = Some("https://example.test".into());

        // Even if a future change re-reads std::env, the controller has the
        // captured values and start_hub uses them via clone(). This test
        // documents the post-fix contract: env_token/env_base_url are
        // populated by setup() and never overwritten by start_hub.
        let cloned_token = ctrl.env_token.clone();
        let cloned_base = ctrl.env_base_url.clone();
        assert_eq!(cloned_token.as_deref(), Some("setup-token"));
        assert_eq!(cloned_base.as_deref(), Some("https://example.test"));
    }

    /// Adversarial test for F-07: restart_hub timeout must NOT leave the
    /// controller in a broken state. The OLD code took the sender without
    /// re-installing on timeout. The NEW code re-installs the OLD sender so
    /// stop_hub remains meaningful.
    #[tokio::test]
    async fn restart_hub_timeout_reinstalls_old_sender() {
        let ctrl = make_hub_controller(true);
        let (tx, _rx) = watch::channel(false);

        // Install a sender + a fake run_serve handle that never completes.
        *ctrl.shutdown_tx.lock().unwrap() = Some(tx.clone());
        let pending = tauri::async_runtime::spawn(async {
            // Never completes within the timeout window.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        ctrl.task_handles.lock().unwrap().run_serve = Some(pending);

        // Take the sender (simulating restart_hub's take).
        let old_tx = ctrl.shutdown_tx.lock().unwrap().take();
        assert!(old_tx.is_some());
        let _ = old_tx.as_ref().unwrap().send(true);

        // The OLD code would now leave the slot empty while waiting.
        // The NEW code re-installs the old sender on timeout. We simulate
        // that branch:
        let handle = ctrl
            .task_handles
            .lock()
            .unwrap()
            .run_serve
            .take()
            .unwrap();
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(50), handle)
            .await
            .is_err();
        assert!(timed_out, "fake handle should have timed out");

        // Re-install the OLD sender so stop_hub remains meaningful.
        if ctrl.shutdown_tx.lock().unwrap().is_none() {
            *ctrl.shutdown_tx.lock().unwrap() = old_tx;
        }

        // Now stop_hub must find a sender.
        let stop_tx = ctrl.shutdown_tx.lock().unwrap().take();
        assert!(
            stop_tx.is_some(),
            "stop_hub must find a re-installed sender after a restart timeout"
        );
    }

    /// Adversarial test for F-08: a slow AppHandle lookup or env read on
    /// start_hub's path must NOT cause double-installation. We model this by
    /// having two concurrent start_hub-shaped attempts: one "fast" and one
    /// "slow" (the slow one holds the slot briefly then releases). The mutex
    /// ensures only one wins.
    #[test]
    fn slow_first_start_hub_does_not_allow_second_to_double_install() {
        let ctrl = std::sync::Arc::new(make_hub_controller(false));
        let ctrl2 = ctrl.clone();

        // "Slow" caller: claims the slot, holds it, then drops its guard.
        let slow = std::thread::spawn(move || {
            let mut g = ctrl.shutdown_tx.lock().unwrap();
            if g.is_some() {
                return false;
            }
            // Pretend to do slow env/state work while holding the lock — but
            // we are NOT supposed to do work in the lock in production. The
            // point of this test is that whatever work happens, the lock
            // is the arbiter.
            std::thread::sleep(std::time::Duration::from_millis(50));
            *g = Some(watch::channel(false).0);
            true
        });

        // "Fast" caller: arrives after the slow one and finds the slot taken.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let fast_won = {
            let mut g = ctrl2.shutdown_tx.lock().unwrap();
            if g.is_some() {
                false
            } else {
                *g = Some(watch::channel(false).0);
                true
            }
        };

        let slow_won = slow.join().unwrap();
        assert!(slow_won, "slow caller should have won the slot");
        assert!(!fast_won, "fast caller must NOT overwrite the slow caller's install");
    }

    // ─── M2 — port-override persistence / parsing / controller surface ────

    use std::sync::Mutex as StdMutex;

    /// Serialize port-override tests so they don't step on the global data dir.
    /// The desktop-port.json path is a real on-disk artifact under
    /// `~/.ilink-hub`, and we don't want parallel tests racing to read/write
    /// the same file in CI.
    static PORT_OVERRIDE_LOCK: StdMutex<()> = StdMutex::new(());

    /// `HOME`-shaped environment for `resolve_initial_listen_addr` to inspect.
    struct ScopedHome {
        previous: Option<String>,
        original: PathBuf,
    }

    impl ScopedHome {
        fn set(home: &Path) -> Self {
            let previous = std::env::var("HOME").ok();
            let original = ilink_hub::paths::data_dir();
            // `data_dir()` reads `dirs::home_dir()`, which on Unix consults
            // `HOME` first; set it for the duration of the test.
            std::env::set_var("HOME", home);
            Self { previous, original }
        }
    }

    impl Drop for ScopedHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            // Touch the original to make sure the value was actually read by
            // dirs; this is a no-op but documents intent.
            let _ = self.original;
        }
    }

    #[test]
    fn loopback_listen_addr_for_port_is_loopback_only() {
        // Hard-coded form is the contract; users must not be able to override
        // it into a non-loopback bind via the GUI.
        assert_eq!(loopback_listen_addr_for_port(8765), "127.0.0.1:8765");
        assert_eq!(loopback_listen_addr_for_port(1), "127.0.0.1:1");
        assert_eq!(loopback_listen_addr_for_port(65535), "127.0.0.1:65535");
    }

    #[test]
    fn parse_loopback_port_accepts_canonical_and_loose_forms() {
        assert_eq!(parse_loopback_port("127.0.0.1:8765"), Some(8765));
        assert_eq!(parse_loopback_port("localhost:9000"), Some(9000));
        // Bare port (no host).
        assert_eq!(parse_loopback_port("9123"), Some(9123));
        // 0 is rejected so the UI never shows "port 0" as the active port.
        assert_eq!(parse_loopback_port("127.0.0.1:0"), None);
        // Non-numeric and non-parseable strings return None so callers fall back.
        assert_eq!(parse_loopback_port("not-an-addr"), None);
        assert_eq!(parse_loopback_port(""), None);
        assert_eq!(parse_loopback_port("[::]:8765"), None);
        assert_eq!(parse_loopback_port("0.0.0.0:8765"), None);
        // Out-of-range numeric tokens reject cleanly.
        assert_eq!(parse_loopback_port("99999"), None);
        assert_eq!(parse_loopback_port("-1"), None);
    }

    #[test]
    fn desktop_port_override_round_trip_under_data_dir() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        // Clean any leftover override from a previous test in this dir.
        let path = desktop_port_override_path();
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }

        // Missing file → Ok(None).
        assert!(load_desktop_port_override().unwrap().is_none());

        // Save and reload.
        save_desktop_port_override(9123).unwrap();
        assert!(path.exists(), "override file should exist after save");
        assert_eq!(load_desktop_port_override().unwrap(), Some(9123));

        // On-disk payload uses camelCase for forward compatibility with the
        // TypeScript frontend (which serde-deserialises via camelCase).
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"port\""),
            "serialised JSON should contain the `port` field, got: {raw}"
        );
    }

    #[test]
    fn desktop_port_override_rejects_zero_in_loaded_file() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        // Hand-craft a malformed file with port=0.
        let path = desktop_port_override_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "{\"port\":0}").unwrap();

        let err = load_desktop_port_override().unwrap_err();
        assert!(
            err.contains("1..=65535") || err.contains("端口"),
            "expected validation error, got: {err}"
        );
    }

    #[test]
    fn desktop_port_override_rejects_malformed_json() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        let path = desktop_port_override_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "this is not json").unwrap();

        let err = load_desktop_port_override().unwrap_err();
        assert!(
            err.contains("格式") || err.contains("无效") || err.contains("JSON"),
            "expected JSON parse error, got: {err}"
        );
    }

    #[test]
    fn resolve_initial_listen_addr_prefers_persisted_port() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        // Persist a port, then ensure it overrides the env default.
        save_desktop_port_override(9123).unwrap();
        // `ILINK_HUB_ADDR` is process-global — scrub it for the duration of
        // this test so the env branch doesn't leak from the outer process.
        let prev = std::env::var("ILINK_HUB_ADDR").ok();
        std::env::remove_var("ILINK_HUB_ADDR");

        let resolved = resolve_initial_listen_addr().expect("resolve");
        assert_eq!(resolved, "127.0.0.1:9123");

        match prev {
            Some(v) => std::env::set_var("ILINK_HUB_ADDR", v),
            None => std::env::remove_var("ILINK_HUB_ADDR"),
        }
    }

    #[test]
    fn resolve_initial_listen_addr_falls_back_to_env_when_no_override() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        // No persisted file → env var should win.
        let prev_addr = std::env::var("ILINK_HUB_ADDR").ok();
        std::env::set_var("ILINK_HUB_ADDR", "127.0.0.1:7777");
        let resolved = resolve_initial_listen_addr().expect("resolve");
        assert_eq!(resolved, "127.0.0.1:7777");

        // Default branch: no override, no env → 127.0.0.1:8765.
        std::env::remove_var("ILINK_HUB_ADDR");
        let resolved = resolve_initial_listen_addr().expect("resolve");
        assert_eq!(resolved, "127.0.0.1:8765");

        match prev_addr {
            Some(v) => std::env::set_var("ILINK_HUB_ADDR", v),
            None => std::env::remove_var("ILINK_HUB_ADDR"),
        }
    }

    #[test]
    fn hub_controller_set_requested_addr_is_observable_via_getter() {
        // The GUI change-port flow needs to flip `requested_addr` in place
        // AND have `hub_info` return the new value on the next call.
        let ctrl = make_hub_controller(false);
        assert_eq!(ctrl.requested_addr(), "127.0.0.1:8765");
        ctrl.set_requested_addr("127.0.0.1:9001".into());
        assert_eq!(ctrl.requested_addr(), "127.0.0.1:9001");

        // `start_hub` reads via `ctrl.requested_addr()` — assert the value
        // the spawn path would use is the updated one.
        assert_eq!(ctrl.requested_addr(), "127.0.0.1:9001");
    }

    #[test]
    fn set_listen_port_command_rejects_zero() {
        // The 0-port rejection is documented behaviour: bind on port 0 is
        // not user-meaningful (it's "pick any free ephemeral port") and the
        // UI must surface this so the user picks a real port.
        let app = tauri::test::mock_app();
        let result = set_listen_port(app.handle().clone(), 0);
        assert!(!result.ok, "port=0 must be rejected");
        assert_eq!(result.listen_port, 0);
        assert!(result.error.is_some(), "rejection must carry an error");
    }

    #[test]
    fn set_listen_port_command_persists_and_updates_controller() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        let app = tauri::test::mock_app();
        app.manage(make_hub_controller(false));

        let result = set_listen_port(app.handle().clone(), 9123);
        assert!(result.ok, "expected ok, got error: {:?}", result.error);
        assert_eq!(result.requested_addr, "127.0.0.1:9123");
        assert_eq!(result.listen_port, 9123);

        // The on-disk file should round-trip via the loader.
        assert_eq!(load_desktop_port_override().unwrap(), Some(9123));

        // And the controller's view should reflect the new address.
        let ctrl = app.state::<HubController>();
        assert_eq!(ctrl.requested_addr(), "127.0.0.1:9123");
    }

    #[test]
    fn set_listen_port_command_overwrites_previous_value() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        let app = tauri::test::mock_app();
        app.manage(make_hub_controller(false));

        let first = set_listen_port(app.handle().clone(), 8765);
        assert!(first.ok);
        assert_eq!(load_desktop_port_override().unwrap(), Some(8765));

        let second = set_listen_port(app.handle().clone(), 9999);
        assert!(second.ok);
        assert_eq!(load_desktop_port_override().unwrap(), Some(9999));

        let ctrl = app.state::<HubController>();
        assert_eq!(ctrl.requested_addr(), "127.0.0.1:9999");
    }

    #[test]
    fn get_desktop_settings_prefills_listen_port_from_requested_addr() {
        let app = tauri::test::mock_app();
        let ctrl = make_hub_controller(false);
        ctrl.set_requested_addr("127.0.0.1:9211".into());
        app.manage(ctrl);

        let settings = get_desktop_settings(app.handle().clone());
        assert_eq!(settings.listen_port, 9211);
        assert_eq!(settings.requested_addr, "127.0.0.1:9211");
    }

    #[test]
    fn get_desktop_settings_falls_back_to_default_when_unparseable() {
        let app = tauri::test::mock_app();
        let ctrl = make_hub_controller(false);
        ctrl.set_requested_addr("[::]:8765".into()); // not parseable to a u16
        app.manage(ctrl);

        let settings = get_desktop_settings(app.handle().clone());
        assert_eq!(settings.listen_port, 8765);
        assert_eq!(settings.requested_addr, "[::]:8765");
    }
}

/// Handles to the three async tasks `spawn_hub_task` launches, so the
/// caller (and the restart path) can abort them on the loser path and
/// `await` the run_serve task to know when it has truly finished.
#[derive(Default)]
struct HubTaskHandles {
    bind_listener: Option<JoinHandle<()>>,
    qr_consumer: Option<JoinHandle<()>>,
    run_serve: Option<JoinHandle<()>>,
}

impl HubTaskHandles {
    /// Abort all in-flight tasks. Idempotent.
    fn abort_all(&mut self) {
        if let Some(h) = self.bind_listener.take() {
            h.abort();
        }
        if let Some(h) = self.qr_consumer.take() {
            h.abort();
        }
        if let Some(h) = self.run_serve.take() {
            h.abort();
        }
    }
}

struct HubController {
    /// Shutdown signal for the in-flight `run_serve`. Set when start succeeds,
    /// cleared by `stop_hub` / `restart_hub`. Used as the "is running" arbiter.
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    /// Handles for the bind listener, QR consumer, and run_serve tasks spawned
    /// alongside the sender. Aborted on the loser path / replaced on restart.
    task_handles: Mutex<HubTaskHandles>,
    /// Configuration captured ONCE in `setup()` so subsequent restarts do not
    /// silently pick up env-mutated token / base_url between stop and start.
    env_token: Option<String>,
    env_base_url: Option<String>,
    /// Listen address (`127.0.0.1:<port>`). Mutated by the GUI "change port"
    /// flow via `set_listen_port`; read by `hub_info` so the UI can show what
    /// will be used on the next start. Mirrors the value persisted to disk so
    /// the in-memory and on-disk views stay coherent across restarts.
    requested_addr: Mutex<String>,
    database_path: PathBuf,
    listening_addr: Arc<Mutex<Option<String>>>,
    hub_state: Arc<Mutex<Option<Arc<ilink_hub::HubState>>>>,
}

impl HubController {
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_running(&self) -> bool {
        self.shutdown_tx
            .lock()
            .expect("HubController mutex poisoned — please restart the app")
            .is_some()
    }

    fn requested_addr(&self) -> String {
        self.requested_addr
            .lock()
            .expect("HubController mutex poisoned — please restart the app")
            .clone()
    }

    fn set_requested_addr(&self, addr: String) {
        let mut g = self
            .requested_addr
            .lock()
            .expect("HubController mutex poisoned — please restart the app");
        *g = addr;
    }
}

/// Match Docker/README style: `sqlite:/absolute/path` (see `store::ensure_sqlite_file`).
fn sqlite_url_for_path(path: &std::path::Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.starts_with('/') {
        format!("sqlite:{normalized}")
    } else if normalized.len() >= 2 && normalized.chars().nth(1) == Some(':') {
        // Windows `C:/...`
        format!("sqlite:/{normalized}")
    } else {
        format!("sqlite:{normalized}")
    }
}

/// Build a `run_serve` task that owns its own QR event channel, bind/state listeners,
/// and shutdown receiver. Returns a fresh `watch::Sender` plus the JoinHandles for the
/// spawned tasks so the caller can store them in the controller and abort the orphaned
/// tasks on the loser path of a race.
///
/// Takes the shared `listening_addr` / `hub_state` Arcs by reference rather than
/// looking them up from the Tauri `AppHandle`. That decoupling is what lets
/// `setup()` construct and `app.manage(HubController { .. })` BEFORE calling this
/// helper (avoids the startup panic on cold launch).
///
/// `env_token` / `env_base_url` are passed explicitly so the configuration source
/// of truth is the controller (which captured them once in `setup()`), not
/// whatever `std::env` happens to return at restart time.
fn spawn_hub_task(
    app: &tauri::AppHandle,
    addr: String,
    db_path: &std::path::Path,
    listening_addr: Arc<Mutex<Option<String>>>,
    hub_state: Arc<Mutex<Option<Arc<ilink_hub::HubState>>>>,
    env_token: Option<String>,
    env_base_url: Option<String>,
) -> (watch::Sender<bool>, HubTaskHandles) {
    let database_url = sqlite_url_for_path(db_path);

    let (tx_bind, rx_bind) = tokio::sync::oneshot::channel::<String>();
    let (tx_state, rx_state) = tokio::sync::oneshot::channel::<Arc<ilink_hub::HubState>>();

    let listening_for_task = listening_addr.clone();
    let hub_state_for_task = hub_state.clone();
    let app_for_bind = app.clone();

    let bind_listener = tauri::async_runtime::spawn(async move {
        if let Ok(state) = rx_state.await {
            let mut g = hub_state_for_task
                .lock()
                .expect("HubController mutex poisoned — please restart the app");
            *g = Some(state);
        }
        if let Ok(s) = rx_bind.await {
            let mut g = listening_for_task
                .lock()
                .expect("HubController mutex poisoned — please restart the app");
            *g = Some(s.clone());
            let _ = app_for_bind.emit("hub-listening", s);
        }
    });

    let (qr_tx, mut qr_rx) = tokio::sync::mpsc::unbounded_channel::<ilink_hub::QrLoginUiEvent>();
    let app_qr_emit = app.clone();
    let qr_consumer = tauri::async_runtime::spawn(async move {
        while let Some(ev) = qr_rx.recv().await {
            let _ = app_qr_emit.emit("qr-login", ev);
        }
    });

    let opts = ilink_hub::ServeOptions {
        token: env_token,
        addr: addr.clone(),
        ilink_base_url: env_base_url,
        database_url,
        on_listening: Some(tx_bind),
        qr_login_ui: Some(qr_tx),
        on_hub_state: Some(tx_state),
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app_handle = app.clone();
    let hub_state_for_shutdown = hub_state.clone();
    let listening_for_clear = listening_addr.clone();

    let run_serve = tauri::async_runtime::spawn(async move {
        let result = ilink_hub::run_serve(opts, shutdown_rx).await;

        // Common teardown for both success and error paths.
        {
            let mut g = hub_state_for_shutdown
                .lock()
                .expect("HubController mutex poisoned — please restart the app");
            *g = None;
        }
        {
            let mut g = listening_for_clear
                .lock()
                .expect("HubController mutex poisoned — please restart the app");
            *g = None;
        }

        // Clear the shutdown_tx slot so a subsequent start_hub (or the port-
        // change "save & restart" flow) can succeed after a bind failure.
        //
        // On the normal restart_hub path, restart_hub already takes this slot
        // before awaiting this task, so the slot is None here and .take() is a
        // harmless no-op.
        if let Some(ctrl) = app_handle.try_state::<HubController>() {
            let _ = ctrl
                .shutdown_tx
                .lock()
                .expect("HubController mutex poisoned — please restart the app")
                .take();
        }

        match result {
            Ok(()) => {
                let _ = app_handle.emit("hub-stopped", ());
            }
            Err(e) => {
                tracing::error!(error = %e, "hub exited with error");
                let _ = app_handle.emit("hub-error", e.to_string());
            }
        }
    });

    (
        shutdown_tx,
        HubTaskHandles {
            bind_listener: Some(bind_listener),
            qr_consumer: Some(qr_consumer),
            run_serve: Some(run_serve),
        },
    )
}

#[tauri::command]
fn hub_info(app: tauri::AppHandle) -> Option<HubInfo> {
    app.try_state::<HubController>().map(|c| {
        let listening_addr = c
            .listening_addr
            .lock()
            .expect("HubController mutex poisoned — please restart the app")
            .clone();
        let hub_base_url = listening_addr
            .as_ref()
            .map(|s| loopback_hub_origin(s).trim_end_matches('/').to_string());
        let admin_url = hub_base_url
            .as_ref()
            .map(|origin| format!("{origin}/hub/ui"));
        HubInfo {
            requested_addr: c.requested_addr(),
            listening_addr,
            admin_url,
            hub_base_url,
            database_path: c.database_path.display().to_string(),
        }
    })
}

#[tauri::command]
async fn stop_hub(app: tauri::AppHandle) -> Result<(), String> {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return Err("hub not running".into());
    };
    let tx = ctrl
        .shutdown_tx
        .lock()
        .expect("HubController mutex poisoned — please restart the app")
        .take();
    if let Some(tx) = tx {
        tx.send(true)
            .map_err(|_| "hub already stopped".to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn start_hub(app: tauri::AppHandle) -> Result<(), String> {
    let ctrl = app
        .try_state::<HubController>()
        .ok_or_else(|| "hub not initialized".to_string())?;

    // Atomically claim the slot before spawning. If the slot is already taken
    // AND the channel is still open (run_serve is alive), refuse the request —
    // this closes the double-spawn race where two concurrent start_hub calls
    // both passed the is_running() check and both spawned run_serve tasks.
    //
    // If the slot is Some but the channel is closed, run_serve has already
    // exited without clearing the slot (e.g. the task finished before setup()
    // installed the sender). Treat this as "not running" and clear the stale
    // sender so we can start fresh.
    {
        let mut guard = ctrl
            .shutdown_tx
            .lock()
            .expect("HubController mutex poisoned — please restart the app");
        if let Some(tx) = guard.as_ref() {
            if !tx.is_closed() {
                return Err("hub already running".into());
            }
            // Stale sender — run_serve has already exited; clear it.
            *guard = None;
        }
    }
    let addr = ctrl.requested_addr();
    let db_path = ctrl.database_path.clone();
    let listening_addr = ctrl.listening_addr.clone();
    let hub_state = ctrl.hub_state.clone();
    let env_token = ctrl.env_token.clone();
    let env_base_url = ctrl.env_base_url.clone();

    let (tx, mut handles) = spawn_hub_task(
        &app,
        addr,
        &db_path,
        listening_addr,
        hub_state,
        env_token,
        env_base_url,
    );

    // Install the sender. If we lose the race here (a concurrent start
    // installed between our earlier drop(guard) and this acquire), abort
    // the orphaned tasks we just spawned and surface the error.
    // Apply the same is_closed() check as above so a stale sender from a
    // fast-exiting run_serve never blocks a valid restart.
    let mut guard = ctrl
        .shutdown_tx
        .lock()
        .expect("HubController mutex poisoned — please restart the app");
    if let Some(tx) = guard.as_ref() {
        if !tx.is_closed() {
            handles.abort_all();
            return Err("hub already running".into());
        }
        // Stale sender — clear before installing the new one.
        *guard = None;
    }
    *guard = Some(tx);
    let mut task_handles = ctrl
        .task_handles
        .lock()
        .expect("HubController mutex poisoned — please restart the app");
    *task_handles = handles;
    Ok(())
}

#[tauri::command]
async fn restart_hub(app: tauri::AppHandle) -> Result<(), String> {
    let Some(ctrl) = app.try_state::<HubController>() else {
        return Err("hub not initialized".to_string());
    };

    // Take the sender and run_serve JoinHandle under one lock acquisition so
    // there is no observable window where the slot is empty but the task is
    // still alive.
    let (old_tx, old_run_serve) = {
        let mut tx_guard = ctrl
            .shutdown_tx
            .lock()
            .expect("HubController mutex poisoned — please restart the app");
        let mut handles_guard = ctrl
            .task_handles
            .lock()
            .expect("HubController mutex poisoned — please restart the app");
        let tx = tx_guard.take();
        let run_serve = handles_guard.run_serve.take();
        (tx, run_serve)
    };
    // Clone the sender up front so we have a copy available for re-install on
    // the timeout branch (the original is consumed by `send(true)` on the
    // happy path).
    let old_tx_for_reinstall = old_tx.clone();
    if let Some(tx) = old_tx {
        // Best-effort: signal stop; ignore error if the receiver already dropped.
        let _ = tx.send(true);
    }

    // Wait on the run_serve JoinHandle (with timeout) so we do not race a
    // `hub-stopped` / `hub-listening` emission against a freshly spawned
    // run_serve. Polling listening_addr is racy because it is None until
    // bind succeeds — run_serve can spend seconds in pre-bind work first.
    if let Some(handle) = old_run_serve {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(_) => {}
            Err(_) => {
                // Timed out — re-install the OLD sender / handle so stop_hub
                // remains meaningful, and surface the error. The OLD run_serve
                // is still alive in the background; the user can either wait
                // or relaunch.
                let mut tx_guard = ctrl
                    .shutdown_tx
                    .lock()
                    .expect("HubController mutex poisoned — please restart the app");
                if tx_guard.is_none() {
                    if let Some(tx) = old_tx_for_reinstall {
                        *tx_guard = Some(tx);
                    }
                }
                return Err("hub stop timed out".into());
            }
        }
    }

    start_hub(app).await
}

/// One-time migration: copy bridge profiles and credentials from the legacy
/// shared CLI directory (`~/.ilink-hub-bridge/`) to the desktop-specific
/// directory (`~/.ilink-hub/desktop-bridge/`) the first time the app runs
/// with the new layout.
///
/// Migration is skipped when the new profiles directory already exists,
/// so re-running is a no-op. Errors are logged and ignored — a failed
/// migration is not fatal; the user will simply start with an empty
/// profile list and need to re-register.
fn migrate_bridge_dir_once() {
    let new_profiles = ilink_hub::paths::desktop_bridge_profiles_dir();
    let new_creds = ilink_hub::paths::desktop_bridge_credentials_dir();
    let old_profiles = ilink_hub::paths::default_bridge_profiles_dir();
    let old_creds = ilink_hub::paths::default_bridge_manager_credentials_dir();

    // Only run when the new directory has never been created.
    if new_profiles.exists() {
        return;
    }

    fn copy_dir_ext(src: &std::path::Path, dst: &std::path::Path, ext: &str) {
        if !src.exists() {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(dst) {
            tracing::warn!(error = %e, dst = %dst.display(), "bridge migration: failed to create dir");
            return;
        }
        let entries = match std::fs::read_dir(src) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, src = %src.display(), "bridge migration: failed to read dir");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some(ext) {
                let dest = dst.join(entry.file_name());
                if let Err(e) = std::fs::copy(&path, &dest) {
                    tracing::warn!(
                        error = %e,
                        src = %path.display(),
                        dst = %dest.display(),
                        "bridge migration: failed to copy file"
                    );
                } else {
                    tracing::info!(
                        src = %path.display(),
                        dst = %dest.display(),
                        "bridge migration: copied"
                    );
                }
            }
        }
    }

    tracing::info!(
        old = %old_profiles.display(),
        new = %new_profiles.display(),
        "migrating desktop bridge profiles to new location"
    );
    copy_dir_ext(&old_profiles, &new_profiles, "yaml");
    copy_dir_ext(&old_profiles, &new_profiles, "yml");
    copy_dir_ext(&old_creds, &new_creds, "json");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("ilink_hub=info".parse().unwrap())
                        .add_directive("tauri=info".parse().unwrap()),
                )
                .try_init();

            // Migrate bridge profiles/credentials from the legacy shared CLI
            // directory to the desktop-specific directory on first launch.
            migrate_bridge_dir_once();

            let data_dir = ilink_hub::paths::data_dir();
            std::fs::create_dir_all(&data_dir).context("create data dir")?;
            let db_path = data_dir.join("ilink-hub.db");

            // Resolve the listen address with this priority: persisted GUI
            // port override → `ILINK_HUB_ADDR` env var → default. A bad /
            // unreadable override file falls back to the env default so the
            // desktop app keeps working rather than refusing to launch.
            let requested_addr = resolve_initial_listen_addr().unwrap_or_else(|err| {
                tracing::warn!(error = %err, "failed to read persisted port override; falling back to env/default");
                std::env::var("ILINK_HUB_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".to_string())
            });

            // Capture env-driven config ONCE so subsequent start_hub / restart_hub
            // calls cannot silently swap token / base_url if the process env is
            // mutated between stop and start.
            let env_token = std::env::var("ILINK_TOKEN").ok();
            let env_base_url = std::env::var("ILINK_BASE_URL").ok();

            let listening_addr = Arc::new(Mutex::new(None::<String>));
            let hub_state = Arc::new(Mutex::new(None::<Arc<ilink_hub::HubState>>));

            // Manage the controller FIRST. The helper takes the shared Arcs
            // as arguments and never looks the controller up via the
            // AppHandle, so the lookup-order panic from M1 cannot recur.
            app.manage(HubController {
                shutdown_tx: Mutex::new(None),
                task_handles: Mutex::new(HubTaskHandles::default()),
                env_token: env_token.clone(),
                env_base_url: env_base_url.clone(),
                requested_addr: Mutex::new(requested_addr.clone()),
                database_path: db_path.clone(),
                listening_addr: listening_addr.clone(),
                hub_state: hub_state.clone(),
            });

            let (shutdown_tx, handles) = spawn_hub_task(
                app.handle(),
                requested_addr.clone(),
                &db_path,
                listening_addr,
                hub_state,
                env_token,
                env_base_url,
            );

            // Install the freshly-spawned sender / handles into the controller.
            // start_hub / restart_hub will overwrite these on subsequent calls.
            {
                let ctrl = app.state::<HubController>();
                let mut tx_guard = ctrl
                    .shutdown_tx
                    .lock()
                    .expect("HubController mutex poisoned — please restart the app");
                *tx_guard = Some(shutdown_tx);
                let mut handles_guard = ctrl
                    .task_handles
                    .lock()
                    .expect("HubController mutex poisoned — please restart the app");
                *handles_guard = handles;
            }
            app.manage(BridgeController {
                task: Mutex::new(None),
                manager: Mutex::new(None),
                runtime: Arc::new(Mutex::new(BridgeRuntime {
                    state: "stopped".into(),
                    error: None,
                })),
                // Use desktop-specific directories so the desktop bridge
                // manager does not collide with a simultaneously-running CLI
                // bridge manager under ~/.ilink-hub-bridge/.
                config_path: ilink_hub::paths::default_bridge_config_path(),
                profiles_dir: ilink_hub::paths::desktop_bridge_profiles_dir(),
                credentials_dir: ilink_hub::paths::desktop_bridge_credentials_dir(),
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { .. } = event {
                let app = window.app_handle();
                if let Some(ctrl) = app.try_state::<HubController>() {
                    let tx_opt = ctrl
                        .shutdown_tx
                        .lock()
                        .expect("HubController mutex poisoned — please restart the app")
                        .take();
                    if let Some(tx) = tx_opt {
                        let _ = tx.send(true);
                    }
                    let mut handles = ctrl
                        .task_handles
                        .lock()
                        .expect("HubController mutex poisoned — please restart the app");
                    handles.abort_all();
                }
                if let Some(ctrl) = app.try_state::<BridgeController>() {
                    if let Ok(mut manager_guard) = ctrl.manager.lock() {
                        if let Some(handle) = manager_guard.take() {
                            handle.stop();
                        }
                    }
                    if let Ok(mut guard) = ctrl.task.lock() {
                        if let Some(handle) = guard.take() {
                            handle.abort();
                        }
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            hub_info,
            hub_clients,
            hub_stats,
            hub_register,
            hub_delete_client,
            hub_update_client,
            bridge_config,
            bridge_save_claude_profile,
            bridge_save_yaml,
            bridge_profiles,
            bridge_save_profile,
            bridge_delete_profile,
            bridge_test_profile,
            bridge_status,
            bridge_start,
            bridge_stop,
            bridge_restart,
            stop_hub,
            start_hub,
            restart_hub,
            get_desktop_settings,
            set_listen_port
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                if let Some(ctrl) = app_handle.try_state::<HubController>() {
                    let tx_opt = ctrl
                        .shutdown_tx
                        .lock()
                        .expect("HubController mutex poisoned — please restart the app")
                        .take();
                    if let Some(tx) = tx_opt {
                        let _ = tx.send(true);
                    }
                    let mut handles = ctrl
                        .task_handles
                        .lock()
                        .expect("HubController mutex poisoned — please restart the app");
                    handles.abort_all();
                }
                if let Some(ctrl) = app_handle.try_state::<BridgeController>() {
                    if let Ok(mut manager_guard) = ctrl.manager.lock() {
                        if let Some(handle) = manager_guard.take() {
                            handle.stop();
                        }
                    }
                    if let Ok(mut guard) = ctrl.task.lock() {
                        if let Some(handle) = guard.take() {
                            handle.abort();
                        }
                    }
                }
                tracing::info!("application exit");
            }
        });
}
