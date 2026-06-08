//! iLink Hub desktop shell: embeds the same runtime as `ilink-hub serve`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
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
    let Some(listen) = ctrl
        .listening_addr
        .lock()
        .ok()
        .and_then(|g| g.clone())
    else {
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
                    clients_online: parse_prometheus_simple_counter(&body, "ilink_hub_clients_online"),
                    clients_total: parse_prometheus_simple_counter(&body, "ilink_hub_clients_total"),
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
                    error: Some("Hub 已启用 ILINK_ADMIN_TOKEN，桌面端需在相同环境变量下启动才能注册。".into()),
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
                Ok(body) => register_err(
                    false,
                    body.errmsg.unwrap_or_else(|| "注册失败".to_string()),
                ),
                Err(e) => register_err(false, format!("解析响应失败: {e}")),
            }
        }
        Err(e) => register_err(false, format!("请求失败: {e}")),
    }
}

struct HubController {
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    requested_addr: String,
    database_path: PathBuf,
    listening_addr: Arc<Mutex<Option<String>>>,
}

fn desktop_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ilink-hub-desktop")
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
        let admin_url = hub_base_url.as_ref().map(|origin| format!("{origin}/hub/ui"));
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

            let data_dir = desktop_data_dir();
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

            let (qr_tx, mut qr_rx) = tokio::sync::mpsc::unbounded_channel::<ilink_hub::QrLoginUiEvent>();
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
            }
        })
        .invoke_handler(tauri::generate_handler![
            hub_info,
            hub_clients,
            hub_stats,
            hub_register,
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
                tracing::info!("application exit");
            }
        });
}
