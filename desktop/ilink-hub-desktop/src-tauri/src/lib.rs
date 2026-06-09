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
    pub timeout_secs: u64,
    pub max_reply_chars: usize,
    pub model: Option<String>,
    pub include_new_session: Option<bool>,
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
    pub cwd: String,
    pub timeout_secs: u64,
    pub max_reply_chars: usize,
    pub model: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub yaml: Option<String>,
    pub include_new_session: Option<bool>,
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

fn build_claude_profile_yaml(req: &SaveClaudeProfileRequest) -> Result<String, String> {
    let cwd = req.cwd.trim();
    if cwd.is_empty() {
        return Err("请填写项目目录".into());
    }
    if req.timeout_secs == 0 {
        return Err("超时时间必须大于 0 秒".into());
    }
    if req.max_reply_chars == 0 {
        return Err("最大回复长度必须大于 0".into());
    }

    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model_env = model
        .map(|m| format!("    env:\n      ILINK_CLAUDE_MODEL: {}\n", yaml_quote(m)))
        .unwrap_or_default();
    let model_env_new = model
        .map(|m| format!("      ILINK_CLAUDE_MODEL: {}\n", yaml_quote(m)))
        .unwrap_or_default();

    if !req.include_new_session.unwrap_or(true) {
        return Ok(format!(
            r#"profiles:
  claude:
    type: claude-code
    cwd: {cwd}
    timeout_secs: {timeout_secs}
    max_reply_chars: {max_reply_chars}
    truncation_suffix: "\n\n...(内容已截断，可缩小问题范围或分多条发送)"
{model_env}
routing:
  strategy: fixed
  default_profile: claude

skip_bot_messages: true
require_text: true
send_error_reply: true
"#,
            cwd = yaml_quote(cwd),
            timeout_secs = req.timeout_secs,
            max_reply_chars = req.max_reply_chars,
            model_env = model_env,
        ));
    }

    Ok(format!(
        r#"profiles:
  claude:
    type: claude-code
    cwd: {cwd}
    timeout_secs: {timeout_secs}
    max_reply_chars: {max_reply_chars}
    truncation_suffix: "\n\n...(内容已截断，可缩小问题范围或分多条发送)"
{model_env}
  claude_new:
    type: claude-code
    cwd: {cwd}
    timeout_secs: {timeout_secs}
    max_reply_chars: {max_reply_chars}
    truncation_suffix: "\n\n...(内容已截断)"
    env:
      ILINK_SESSION_ID: ""
{model_env_new}
routing:
  strategy: prefix
  default_profile: claude
  prefix_rules:
    - prefix: "/new "
      profile: claude_new

skip_bot_messages: true
require_text: true
send_error_reply: true
"#,
        cwd = yaml_quote(cwd),
        timeout_secs = req.timeout_secs,
        max_reply_chars = req.max_reply_chars,
        model_env = model_env,
        model_env_new = model_env_new,
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

fn build_command_profile_yaml(
    command: &str,
    args: &[String],
    cwd: &str,
    timeout_secs: u64,
    max_reply_chars: usize,
) -> Result<String, String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("请填写项目目录".into());
    }
    if command.trim().is_empty() {
        return Err("请填写命令".into());
    }
    if timeout_secs == 0 {
        return Err("超时时间必须大于 0 秒".into());
    }
    if max_reply_chars == 0 {
        return Err("最大回复长度必须大于 0".into());
    }
    Ok(format!(
        r#"command: {command}
args: {args}
stdin: none
cwd: {cwd}
timeout_secs: {timeout_secs}
max_reply_chars: {max_reply_chars}
"#,
        command = yaml_quote(command.trim()),
        args = yaml_string_array(args),
        cwd = yaml_quote(cwd),
        timeout_secs = timeout_secs,
        max_reply_chars = max_reply_chars
    ))
}

fn build_bridge_profile_yaml(req: &SaveBridgeProfileRequest) -> Result<String, String> {
    match req.template.as_str() {
        "claude" => build_claude_profile_yaml(&SaveClaudeProfileRequest {
            cwd: req.cwd.clone(),
            timeout_secs: req.timeout_secs,
            max_reply_chars: req.max_reply_chars,
            model: req.model.clone(),
            include_new_session: req.include_new_session,
        }),
        "cursor" => {
            let args = req
                .args
                .clone()
                .unwrap_or_else(|| vec!["-p".into(), "{{MESSAGE}}".into()]);
            build_command_profile_yaml(
                req.command.as_deref().unwrap_or("agent"),
                &args,
                &req.cwd,
                req.timeout_secs,
                req.max_reply_chars,
            )
        }
        "codex" => {
            let args = req
                .args
                .clone()
                .unwrap_or_else(|| vec!["exec".into(), "{{MESSAGE}}".into()]);
            build_command_profile_yaml(
                req.command.as_deref().unwrap_or("codex"),
                &args,
                &req.cwd,
                req.timeout_secs,
                req.max_reply_chars,
            )
        }
        "gemini" => {
            let args = req
                .args
                .clone()
                .unwrap_or_else(|| vec!["-p".into(), "{{MESSAGE}}".into()]);
            build_command_profile_yaml(
                req.command.as_deref().unwrap_or("gemini"),
                &args,
                &req.cwd,
                req.timeout_secs,
                req.max_reply_chars,
            )
        }
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
    let yaml = build_claude_profile_yaml(&req)?;
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
fn bridge_start(app: tauri::AppHandle) -> Result<(), String> {
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
fn bridge_restart(app: tauri::AppHandle) -> Result<(), String> {
    bridge_stop(app.clone())?;
    start_bridge_task(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_profile_wizard_generates_valid_bridge_yaml() {
        let yaml = build_claude_profile_yaml(&SaveClaudeProfileRequest {
            cwd: "/tmp/my project".into(),
            timeout_secs: 600,
            max_reply_chars: 8000,
            model: Some("sonnet".into()),
            include_new_session: Some(true),
        })
        .unwrap();
        let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();

        assert_eq!(app.default_profile_name(), "claude");
        assert_eq!(app.routing_label(), "prefix");
        let profile = app.profile("claude").unwrap();
        assert_eq!(profile.cwd.as_deref(), Some("/tmp/my project"));
        assert_eq!(
            profile.env.get("ILINK_CLAUDE_MODEL").map(String::as_str),
            Some("sonnet")
        );
    }

    #[test]
    fn claude_profile_wizard_requires_cwd() {
        let err = build_claude_profile_yaml(&SaveClaudeProfileRequest {
            cwd: " ".into(),
            timeout_secs: 600,
            max_reply_chars: 8000,
            model: None,
            include_new_session: Some(true),
        })
        .unwrap_err();

        assert!(err.contains("项目目录"));
    }

    #[test]
    fn claude_profile_wizard_can_disable_new_session_route() {
        let yaml = build_claude_profile_yaml(&SaveClaudeProfileRequest {
            cwd: "/tmp/my project".into(),
            timeout_secs: 600,
            max_reply_chars: 8000,
            model: None,
            include_new_session: Some(false),
        })
        .unwrap();
        let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();
        assert_eq!(app.routing_label(), "fixed");
        assert!(app.profile("claude_new").is_none());
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
                timeout_secs: 600,
                max_reply_chars: 8000,
                model: None,
                command: None,
                args: None,
                yaml: None,
                include_new_session: None,
            })
            .unwrap();
            let app = ilink_hub::bridge::BridgeApp::parse_yaml(&yaml).unwrap();
            let profile = app.profile("default").unwrap();
            assert_eq!(profile.command, command);
            assert_eq!(profile.cwd.as_deref(), Some("/tmp/project"));
        }
    }
}

struct HubController {
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    requested_addr: String,
    database_path: PathBuf,
    listening_addr: Arc<Mutex<Option<String>>>,
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

#[tauri::command]
fn hub_info(app: tauri::AppHandle) -> Option<HubInfo> {
    app.try_state::<HubController>().map(|c| {
        let listening_addr = c.listening_addr.lock().ok().and_then(|g| g.clone());
        let hub_base_url = listening_addr
            .as_ref()
            .map(|s| loopback_hub_origin(s).trim_end_matches('/').to_string());
        let admin_url = hub_base_url
            .as_ref()
            .map(|origin| format!("{origin}/hub/ui"));
        HubInfo {
            requested_addr: c.requested_addr.clone(),
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
    let tx = ctrl.shutdown_tx.lock().map_err(|e| e.to_string())?.take();
    if let Some(tx) = tx {
        tx.send(true)
            .map_err(|_| "hub already stopped".to_string())?;
    }
    Ok(())
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

            let data_dir = ilink_hub::paths::data_dir();
            std::fs::create_dir_all(&data_dir).context("create data dir")?;
            let db_path = data_dir.join("ilink-hub.db");

            let requested_addr =
                std::env::var("ILINK_HUB_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".to_string());
            let database_url = sqlite_url_for_path(&db_path);
            let token = std::env::var("ILINK_TOKEN").ok();
            let ilink_base_url = std::env::var("ILINK_BASE_URL").ok();

            let (tx_bind, rx_bind) = tokio::sync::oneshot::channel::<String>();
            let listening_addr = Arc::new(Mutex::new(None::<String>));
            let listening_for_task = listening_addr.clone();
            let app_for_bind = app.handle().clone();

            tauri::async_runtime::spawn(async move {
                if let Ok(s) = rx_bind.await {
                    if let Ok(mut g) = listening_for_task.lock() {
                        *g = Some(s.clone());
                    }
                    let _ = app_for_bind.emit("hub-listening", s);
                }
            });

            let (qr_tx, mut qr_rx) =
                tokio::sync::mpsc::unbounded_channel::<ilink_hub::QrLoginUiEvent>();
            let app_qr_emit = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = qr_rx.recv().await {
                    let _ = app_qr_emit.emit("qr-login", ev);
                }
            });

            let opts = ilink_hub::ServeOptions {
                token,
                addr: requested_addr.clone(),
                ilink_base_url,
                database_url,
                on_listening: Some(tx_bind),
                qr_login_ui: Some(qr_tx),
            };

            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let app_handle = app.handle().clone();

            tauri::async_runtime::spawn(async move {
                match ilink_hub::run_serve(opts, shutdown_rx).await {
                    Ok(()) => {
                        let _ = app_handle.emit("hub-stopped", ());
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "hub exited with error");
                        let _ = app_handle.emit("hub-error", e.to_string());
                    }
                }
            });

            app.manage(HubController {
                shutdown_tx: Mutex::new(Some(shutdown_tx)),
                requested_addr: requested_addr.clone(),
                database_path: db_path,
                listening_addr,
            });
            app.manage(BridgeController {
                task: Mutex::new(None),
                manager: Mutex::new(None),
                runtime: Arc::new(Mutex::new(BridgeRuntime {
                    state: "stopped".into(),
                    error: None,
                })),
                config_path: ilink_hub::paths::default_bridge_config_path(),
                profiles_dir: ilink_hub::paths::default_bridge_profiles_dir(),
                credentials_dir: ilink_hub::paths::default_bridge_manager_credentials_dir(),
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { .. } = event {
                let app = window.app_handle();
                if let Some(ctrl) = app.try_state::<HubController>() {
                    if let Ok(mut guard) = ctrl.shutdown_tx.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(true);
                        }
                    }
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
            bridge_config,
            bridge_save_claude_profile,
            bridge_save_yaml,
            bridge_profiles,
            bridge_save_profile,
            bridge_delete_profile,
            bridge_status,
            bridge_start,
            bridge_stop,
            bridge_restart,
            stop_hub
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                if let Some(ctrl) = app_handle.try_state::<HubController>() {
                    if let Ok(mut guard) = ctrl.shutdown_tx.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(true);
                        }
                    }
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
