//! Hub client/stats/register Tauri commands.

use std::sync::Arc;

use tauri::Manager;

use crate::listen_addr::loopback_hub_origin;
use crate::HubController;

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

#[tauri::command]
pub(crate) async fn hub_clients(app: tauri::AppHandle) -> HubClientsPayload {
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
pub(crate) fn parse_prometheus_simple_counter(body: &str, name: &str) -> Option<u64> {
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
pub(crate) async fn hub_stats(app: tauri::AppHandle) -> HubStatsPayload {
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

pub(crate) fn register_err(auth_required: bool, error: impl Into<String>) -> RegisterResult {
    RegisterResult {
        ok: false,
        vtoken: None,
        base_url: None,
        auth_required,
        error: Some(error.into()),
    }
}

#[tauri::command]
pub(crate) async fn hub_register(
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

pub(crate) fn delete_client_err(auth_required: bool, error: impl Into<String>) -> DeleteClientResult {
    DeleteClientResult {
        ok: false,
        auth_required,
        error: Some(error.into()),
    }
}

pub(crate) fn hub_state_from_app(
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
pub(crate) async fn hub_delete_client(app: tauri::AppHandle, name: String) -> DeleteClientResult {
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

pub(crate) fn update_client_err(auth_required: bool, error: impl Into<String>) -> UpdateClientResult {
    UpdateClientResult {
        ok: false,
        name: None,
        auth_required,
        error: Some(error.into()),
    }
}

#[tauri::command]
pub(crate) async fn hub_update_client(
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

    let (persona_name, persona_emoji) = {
        let registry = state.clients.registry.read().await;
        match registry.get_by_name(&old_name) {
            Some(info) => (info.persona_name.clone(), info.persona_emoji.clone()),
            None => {
                return update_client_err(false, format!("未找到后端「{old_name}」"));
            }
        }
    };

    // Preserve existing persona fields — the desktop UI does not edit them yet,
    // and update_client_in_hub writes persona to DB unconditionally (None clears).
    match update_client_in_hub(
        state.as_ref(),
        &old_name,
        &name,
        label,
        persona_name,
        persona_emoji,
    )
    .await
    {
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

