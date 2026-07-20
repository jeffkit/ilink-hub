//! Bridge profile YAML builders and Tauri bridge commands.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::async_runtime::JoinHandle;
use tauri::Manager;

use crate::listen_addr::loopback_hub_origin;
use crate::HubController;

#[derive(Clone, Default)]
pub(crate) struct BridgeRuntime {
    pub(crate) state: String,
    pub(crate) error: Option<String>,
}

pub(crate) struct BridgeController {
    pub(crate) task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) manager: Mutex<Option<im_agentproc::bridge::manager::BridgeManagerHandle>>,
    pub(crate) runtime: Arc<Mutex<BridgeRuntime>>,
    pub(crate) config_path: PathBuf,
    pub(crate) profiles_dir: PathBuf,
    pub(crate) credentials_dir: PathBuf,
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
    pub manager: Option<im_agentproc::bridge::manager::BridgeManagerStatus>,
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

pub(crate) fn yaml_quote(s: &str) -> String {
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
pub(crate) fn env_vars_yaml(env_vars: &[EnvVar], indent: usize) -> String {
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

/// Build the minimal `claude-code` profile YAML (spec-aligned hub form).
pub(crate) fn build_claude_profile_yaml(cwd: &str, env_vars: &[EnvVar]) -> Result<String, String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("请填写项目目录".into());
    }
    let env_section = env_vars_yaml(env_vars, 2);
    Ok(format!(
        "agentproc:\n  executor: claude-code\n  cwd: {cwd}\n{env_section}",
        cwd = yaml_quote(cwd),
        env_section = env_section,
    ))
}

/// Build a minimal single-profile YAML for CLI tools (codex, cursor agent, gemini, …).
pub(crate) fn build_simple_command_yaml(
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
    let env_section = env_vars_yaml(env_vars, 2);
    Ok(format!(
        "agentproc:\n  command: {command}\n  args: {args}\n  cwd: {cwd}\n{env_section}",
        command = yaml_quote(command.trim()),
        args = yaml_string_array(args),
        cwd = yaml_quote(cwd),
        env_section = env_section,
    ))
}

pub(crate) fn sanitize_profile_file_id(raw: &str) -> Result<String, String> {
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

pub(crate) fn yaml_string_array(items: &[String]) -> String {
    let quoted = items
        .iter()
        .map(|s| yaml_quote(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{quoted}]")
}

pub(crate) fn build_bridge_profile_yaml(req: &SaveBridgeProfileRequest) -> Result<String, String> {
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

pub(crate) fn detect_bridge_profile_template(app: &im_agentproc::bridge::BridgeApp) -> String {
    let p = app.profile(app.default_profile_name());
    if let Some(p) = p {
        if p.executor.as_deref() == Some("claude-code") || p.command == "ilink-hub-bridge" {
            return "claude".into();
        }
        match p.command.as_str() {
            "agent" => return "cursor".into(),
            "codex" => return "codex".into(),
            "gemini" => return "gemini".into(),
            _ => {}
        }
    }
    "custom".into()
}

pub(crate) fn summarize_bridge_profile_file(
    id: String,
    path: PathBuf,
    yaml: String,
) -> DesktopBridgeProfileFile {
    match im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, id.clone()) {
        Ok(app) => {
            let template = detect_bridge_profile_template(&app);
            let profile = app.profile(app.default_profile_name());
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
                .and_then(|p| im_agentproc::bridge::probe_profile_light(p).err())
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
                model: profile.and_then(|p| p.env.get("CLAUDE_MODEL").cloned()),
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

pub(crate) fn summarize_bridge_config(
    path: &std::path::Path,
    yaml: String,
    exists: bool,
) -> BridgeConfigPayload {
    match im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "bridge".to_string()) {
        Ok(app) => {
            let profiles = app
                .profile_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            let claude_profile = app.profile(app.default_profile_name()).and_then(|p| {
                if p.executor.as_deref() == Some("claude-code")
                    || p.command == "ilink-hub-bridge"
                    || p.command == "claude"
                {
                    Some(DesktopBridgeProfileView {
                        name: app.default_profile_name().to_string(),
                        cwd: p.cwd.clone(),
                        timeout_secs: p.timeout_secs,
                        max_reply_chars: p.max_reply_chars,
                        model: p.env.get("CLAUDE_MODEL").cloned(),
                    })
                } else {
                    None
                }
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
pub(crate) async fn bridge_config(app: tauri::AppHandle) -> BridgeConfigPayload {
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
pub(crate) async fn bridge_save_claude_profile(
    app: tauri::AppHandle,
    req: SaveClaudeProfileRequest,
) -> Result<BridgeConfigPayload, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let env_vars = req.env_vars.unwrap_or_default();
    let yaml = build_claude_profile_yaml(&req.cwd, &env_vars)?;
    im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "bridge".to_string())
        .map_err(|e| e.to_string())?;
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
pub(crate) async fn bridge_save_yaml(
    app: tauri::AppHandle,
    yaml: String,
) -> Result<BridgeConfigPayload, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "bridge".to_string())
        .map_err(|e| e.to_string())?;
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

pub(crate) fn profile_path(ctrl: &BridgeController, id: &str) -> Result<PathBuf, String> {
    let id = sanitize_profile_file_id(id)?;
    Ok(ctrl.profiles_dir.join(format!("{id}.yaml")))
}

pub(crate) fn existing_profile_path(ctrl: &BridgeController, id: &str) -> Result<Option<PathBuf>, String> {
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
pub(crate) async fn bridge_profiles(app: tauri::AppHandle) -> BridgeProfilesPayload {
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
pub(crate) async fn bridge_save_profile(
    app: tauri::AppHandle,
    req: SaveBridgeProfileRequest,
) -> Result<DesktopBridgeProfileFile, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let id = sanitize_profile_file_id(&req.id)?;
    let yaml = build_bridge_profile_yaml(&req)?;
    im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "bridge".to_string())
        .map_err(|e| e.to_string())?;
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
pub(crate) async fn bridge_delete_profile(app: tauri::AppHandle, id: String) -> Result<(), String> {
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
pub(crate) async fn bridge_test_profile(
    app: tauri::AppHandle,
    id: String,
    _message: String,
) -> Result<ProbeResult, String> {
    let Some(ctrl) = app.try_state::<BridgeController>() else {
        return Err("Bridge 未初始化".into());
    };
    let config_path =
        existing_profile_path(&ctrl, &id)?.ok_or_else(|| format!("未找到 profile `{id}`"))?;
    let app_cfg = im_agentproc::bridge::BridgeApp::load(&config_path)
        .map_err(|e| format!("加载配置失败: {e}"))?;
    let profile_name = app_cfg.default_profile_name();
    let profile = app_cfg
        .profile(profile_name)
        .ok_or_else(|| format!("配置中未找到默认 profile `{profile_name}`"))?;

    // For safety, hardcode the probe message to "ping" to prevent command injection/RCE.
    let msg = "ping";
    match im_agentproc::bridge::dry_run_profile(profile, msg).await {
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
                im_agentproc::bridge::ProbeError::Unauthenticated(detail)
                | im_agentproc::bridge::ProbeError::ExecutionError(detail) => {
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

pub(crate) fn set_bridge_runtime(ctrl: &BridgeController, state: &str, error: Option<String>) {
    if let Ok(mut runtime) = ctrl.runtime.lock() {
        runtime.state = state.to_string();
        runtime.error = error;
    }
}

pub(crate) fn set_bridge_runtime_arc(runtime: &Arc<Mutex<BridgeRuntime>>, state: &str, error: Option<String>) {
    if let Ok(mut r) = runtime.lock() {
        r.state = state.to_string();
        r.error = error;
    }
}

#[tauri::command]
pub(crate) async fn bridge_status(app: tauri::AppHandle) -> BridgeStatusPayload {
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
    // Clone the handle before releasing the Mutex so we can .await outside the lock.
    let manager_handle = ctrl.manager.lock().ok().and_then(|g| g.as_ref().cloned());
    let manager = match manager_handle {
        Some(h) => Some(h.status().await),
        None => None,
    };
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

pub(crate) fn start_bridge_task(app: &tauri::AppHandle) -> Result<(), String> {
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
    let mut opts = im_agentproc::bridge::manager::BridgeManagerOptions::new(
        hub_url,
        profiles_dir,
        credentials_dir,
    );
    opts.scan_interval = std::time::Duration::from_secs(3);
    let (manager_handle, manager_task) = im_agentproc::bridge::manager::spawn_bridge_manager(opts);
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
pub(crate) async fn bridge_start(app: tauri::AppHandle) -> Result<(), String> {
    start_bridge_task(&app)
}

#[tauri::command]
pub(crate) fn bridge_stop(app: tauri::AppHandle) -> Result<(), String> {
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
pub(crate) async fn bridge_restart(app: tauri::AppHandle) -> Result<(), String> {
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

    #[test]
    fn claude_profile_wizard_generates_minimal_yaml() {
        let yaml = build_claude_profile_yaml("/tmp/my project", &[]).unwrap();
        let app = im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "claude".to_string()).unwrap();

        assert_eq!(app.default_profile_name(), "claude");
        assert_eq!(app.routing_label(), "fixed");
        let profile = app.profile("claude").unwrap();
        assert_eq!(profile.executor.as_deref(), Some("claude-code"));
        assert_eq!(profile.cwd.as_deref(), Some("/tmp/my project"));
    }

    #[test]
    fn claude_profile_with_env_vars() {
        let env_vars = vec![EnvVar {
            key: "CLAUDE_MODEL".into(),
            value: "sonnet".into(),
        }];
        let yaml = build_claude_profile_yaml("/tmp/project", &env_vars).unwrap();
        let app =
            im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "claude".to_string()).unwrap();
        let profile = app.profile("claude").unwrap();
        assert_eq!(
            profile.env.get("CLAUDE_MODEL").map(String::as_str),
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
            let app = im_agentproc::bridge::BridgeApp::parse_yaml(
                &yaml,
                format!("{template}-demo"),
            )
            .unwrap();
            let profile = app.profile(app.default_profile_name()).unwrap();
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
        let app =
            im_agentproc::bridge::BridgeApp::parse_yaml(&yaml, "codex-demo".to_string()).unwrap();
        let profile = app.profile(app.default_profile_name()).unwrap();
        assert_eq!(
            profile.env.get("MY_TOKEN").map(String::as_str),
            Some("abc123")
        );
    }
}
