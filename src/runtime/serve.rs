//! Start the Axum hub and upstream polling loop until `shutdown` becomes `true`.
//!
//! # Shutdown
//!
//! Pass a [`tokio::sync::watch::Receiver`] whose value the caller sets to `true` when the
//! process should stop (e.g. Ctrl+C in the CLI, or app exit in a desktop shell). The same
//! receiver is cloned for the upstream polling task and for Axum graceful shutdown.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::hub::{
    spawn_dispatcher, spawn_health_checker, spawn_quote_index_evictor, HubState, InMemoryQueue,
    MessageQueue,
};
use crate::ilink::{LoginClient, QrLoginUiEvent, SessionRenewal, UpstreamClient};
use crate::server::build_router;
use crate::store::Store;

/// Arguments for [`run_serve`], matching the `ilink-hub serve` CLI flags.
pub struct ServeOptions {
    pub token: Option<String>,
    pub addr: String,
    pub ilink_base_url: Option<String>,
    pub database_url: String,
    /// After [`TcpListener::bind`] succeeds, sends the bound socket display string (e.g.
    /// `127.0.0.1:8765`). Embedders use this to avoid showing a listen address before bind.
    pub on_listening: Option<tokio::sync::oneshot::Sender<String>>,
    /// Optional channel for WeChat QR login UI (desktop); [`None`] keeps terminal-only flow.
    pub qr_login_ui: Option<mpsc::UnboundedSender<QrLoginUiEvent>>,
    /// After [`HubState`] is created, sends a clone for embedders that need in-process admin APIs.
    pub on_hub_state: Option<tokio::sync::oneshot::Sender<Arc<HubState>>>,
}

impl fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeOptions")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("addr", &self.addr)
            .field("ilink_base_url", &self.ilink_base_url)
            .field("database_url", &self.database_url)
            .field("on_listening", &self.on_listening.is_some())
            .field("qr_login_ui", &self.qr_login_ui.is_some())
            .field("on_hub_state", &self.on_hub_state.is_some())
            .finish()
    }
}

/// Run the hub HTTP server until `shutdown` signals `true`.
///
/// Does **not** install a [`tracing`] subscriber; initialize logging in the binary (`main`)
/// or test harness before calling this function.
pub async fn run_serve(opts: ServeOptions, mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
    let ServeOptions {
        token: token_arg,
        addr,
        ilink_base_url,
        database_url,
        on_listening,
        qr_login_ui,
        on_hub_state,
    } = opts;

    info!(%addr, %database_url, "iLink Hub starting");

    let store = Arc::new(Store::connect(&database_url).await?);

    let (token, base_url) = resolve_token(
        token_arg,
        ilink_base_url.clone(),
        store.clone(),
        qr_login_ui.clone(),
    )
    .await?;

    let upstream = Arc::new(UpstreamClient::new(token, Some(base_url.clone())));
    let queue = build_queue_backend()?;
    let state = HubState::new(upstream.clone(), store.clone(), queue, shutdown_rx.clone());

    if let Some(tx) = on_hub_state {
        let _ = tx.send(state.clone());
    }

    load_clients_from_db(state.clone(), store.clone()).await;

    let (tx, rx) = broadcast::channel::<crate::ilink::types::WeixinMessage>(256);

    spawn_dispatcher(state.clone(), rx);

    spawn_health_checker(state.clone());
    spawn_quote_index_evictor(state.clone());

    {
        let upstream_clone = upstream.clone();
        let tx_clone = tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();
        let renewal = Some(SessionRenewal {
            store: store.clone(),
            ilink_base_url: ilink_base_url.or(Some(base_url)),
            qr_login_ui,
            qr_tx: Some(state.qr_tx.clone()),
            qr_last_ready: Some(state.qr_last_ready.clone()),
            ilink_status: Some(state.ilink_status.clone()),
            relogin_rx: Some(state.relogin_tx.subscribe()),
        });
        tokio::spawn(async move {
            upstream_clone
                .run_polling_loop(tx_clone, shutdown_rx_clone, renewal)
                .await;
        });
    }

    let identity = crate::relay::DeviceIdentity::load_or_create()?;
    if crate::relay::relay_enabled() {
        let hub_base = format!("http://{}", crate::relay::hub_loopback_addr(&addr));
        let relay_ws = crate::relay::relay_ws_url();
        let pair_url = crate::relay::resolve_pair_public_url(identity.device_id());
        info!(
            device_id = %identity.device_id(),
            pair_url = %pair_url,
            relay = %relay_ws,
            "pairing relay enabled (zero-config)"
        );
        crate::relay::client::spawn_relay_client(identity, hub_base, relay_ws);
    } else {
        info!("pairing relay disabled (set HUB_PAIR_URL or ILINKHUB_RELAY=0)");
    }

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_display = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    if let Some(tx) = on_listening {
        let _ = tx.send(local_display);
    }
    info!(%addr, "iLink Hub listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            while !*shutdown_rx.borrow() {
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await?;

    info!("iLink Hub stopped");
    Ok(())
}

async fn resolve_token(
    token_arg: Option<String>,
    ilink_base_url: Option<String>,
    store: Arc<Store>,
    qr_login_ui: Option<mpsc::UnboundedSender<QrLoginUiEvent>>,
) -> Result<(String, String)> {
    let quiet_ui = qr_login_ui.is_some();
    let default_base = "https://ilinkai.weixin.qq.com".to_string();

    // Priority: DB token > env/CLI token.
    //
    // The env/CLI token (ILINK_TOKEN) is treated as a bootstrap seed only — it is saved to DB
    // the first time when the DB is empty, and from that point the DB token takes precedence.
    //
    // This prevents ILINK_TOKEN from overwriting a QR-renewed session token on every restart.
    // Without this, the stale env token overwrites the DB after every restart, forcing a fresh
    // QR scan each time the service is restarted.
    let db_creds = store.load_credentials().await?;
    let db_is_empty = db_creds.is_none();

    let (candidate, base) = if let Some((token, base)) = db_creds {
        info!("loaded bot token from database");
        (Some(token), base)
    } else if let Some(token) = token_arg {
        // DB is empty; use the env/CLI token as the initial bootstrap seed.
        let base = ilink_base_url
            .clone()
            .unwrap_or_else(|| default_base.clone());
        (Some(token), base)
    } else {
        (
            None,
            ilink_base_url
                .clone()
                .unwrap_or_else(|| default_base.clone()),
        )
    };

    if let Some(token) = candidate {
        if UpstreamClient::is_well_formed_bot_token(&token) {
            if db_is_empty {
                // Bootstrap: persist the env/CLI token so future restarts load from DB.
                store.save_credentials(&token, &base).await?;
            }
            info!("using iLink token without startup session probe");
            return Ok((token, base));
        }
        warn!("iLink token malformed");
        if !quiet_ui {
            println!();
            println!("⚠️  未检测到有效的 iLink 微信登录态，请扫描下方二维码完成登录。");
            println!();
        }
    } else {
        info!("no iLink token found, starting QR login");
        if !quiet_ui {
            println!();
            println!("首次启动需要绑定微信机器人，请扫描下方二维码登录。");
            println!();
        }
    }

    perform_qr_login(ilink_base_url, store, qr_login_ui, &default_base).await
}

async fn perform_qr_login(
    ilink_base_url: Option<String>,
    store: Arc<Store>,
    qr_login_ui: Option<mpsc::UnboundedSender<QrLoginUiEvent>>,
    default_base: &str,
) -> Result<(String, String)> {
    let login_base = ilink_base_url.clone();
    let login_client = LoginClient::new(ilink_base_url);
    let token = login_client.login_with_qr_ui(qr_login_ui).await?;
    let base = login_base.unwrap_or_else(|| default_base.to_string());
    store.save_credentials(&token, &base).await?;
    info!("iLink login successful, token saved");
    Ok((token, base))
}

async fn load_clients_from_db(state: Arc<HubState>, store: Arc<Store>) {
    match store.list_clients().await {
        Ok(clients) => {
            let count = clients.len();
            let mut registry = state.registry.write().await;
            for c in clients {
                registry.register_with_vtoken(
                    c.name.clone(),
                    c.label.clone(),
                    Some(c.vtoken.clone()),
                );
            }
            info!(count, "loaded clients from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load clients from DB");
        }
    }

    match store.list_routes().await {
        Ok(routes) => {
            let count = routes.len();
            let mut router = state.router.lock().await;
            for (from_user, vtoken) in routes {
                router.set_route(&from_user, vtoken);
            }
            info!(count, "loaded routing state from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load routing state from DB");
        }
    }

    match store.list_recent_context_tokens(500).await {
        Ok(entries) => {
            let count = entries.len();
            let ctx_map = state.ctx_map.write().await;
            for (vctx, real_ctx, peer_user_id) in entries {
                ctx_map.seed_full(vctx, real_ctx, peer_user_id);
            }
            info!(count, "warmed context_token cache from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load context_token cache from DB");
        }
    }
}

/// Lower bound for `ILINK_MAX_QUEUE_SIZE`. Values below this clamp to [`MIN_MAX_QUEUE_SIZE`].
const MIN_MAX_QUEUE_SIZE: usize = 10;
/// Upper bound for `ILINK_MAX_QUEUE_SIZE`. Values above this clamp to [`MAX_MAX_QUEUE_SIZE`].
const MAX_MAX_QUEUE_SIZE: usize = 10_000;

/// Select and initialise the queue backend from the `ILINK_QUEUE_BACKEND` env var.
///
/// Supported values:
/// - `"memory"` or unset — in-memory queue (default)
///
/// Any other value (including `"redis"`, which is not yet implemented) returns `Err` so
/// the process fails fast rather than silently using memory and losing messages on restart.
fn build_queue_backend() -> Result<Arc<dyn MessageQueue>> {
    let max_queue_size = resolve_max_queue_size();
    match std::env::var("ILINK_QUEUE_BACKEND")
        .as_deref()
        .unwrap_or("")
    {
        "memory" | "" => {
            info!(
                backend = "memory",
                max_queue_size = max_queue_size,
                "queue backend initialized"
            );
            Ok(Arc::new(InMemoryQueue::with_limit(max_queue_size)))
        }
        "redis" => {
            anyhow::bail!(
                "ILINK_QUEUE_BACKEND=redis is not yet implemented. \
                 Use 'memory' or leave unset."
            )
        }
        other => {
            anyhow::bail!(
                "Unknown ILINK_QUEUE_BACKEND value {:?}. Supported values: 'memory'.",
                other
            )
        }
    }
}

/// Resolve `ILINK_MAX_QUEUE_SIZE` against the [`MIN_MAX_QUEUE_SIZE`, [`MAX_MAX_QUEUE_SIZE`]
/// range, emitting a warning when the value is out of range or unparseable. Pure function
/// over the env var, exposed for unit tests.
fn resolve_max_queue_size() -> usize {
    let default = crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE;
    let Ok(val) = std::env::var("ILINK_MAX_QUEUE_SIZE") else {
        return default;
    };
    if val.is_empty() {
        warn!("ILINK_MAX_QUEUE_SIZE is empty. Using default: {}.", default);
        return default;
    }
    match val.parse::<usize>() {
        Ok(parsed) if parsed < MIN_MAX_QUEUE_SIZE => {
            warn!(
                "ILINK_MAX_QUEUE_SIZE value {} is out of bounds [{}, {}]. Clamping to {}.",
                parsed, MIN_MAX_QUEUE_SIZE, MAX_MAX_QUEUE_SIZE, MIN_MAX_QUEUE_SIZE
            );
            MIN_MAX_QUEUE_SIZE
        }
        Ok(parsed) if parsed > MAX_MAX_QUEUE_SIZE => {
            warn!(
                "ILINK_MAX_QUEUE_SIZE value {} is out of bounds [{}, {}]. Clamping to {}.",
                parsed, MIN_MAX_QUEUE_SIZE, MAX_MAX_QUEUE_SIZE, MAX_MAX_QUEUE_SIZE
            );
            MAX_MAX_QUEUE_SIZE
        }
        Ok(parsed) => parsed,
        Err(_) => {
            warn!(
                "Invalid ILINK_MAX_QUEUE_SIZE value {:?}. Using default: {}.",
                val, default
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::WeixinMessage;
    use std::sync::Mutex;

    /// Serialises env-mutating tests in this module. The guard must never be held
    /// across an `.await` point — `build_queue_backend` / `resolve_max_queue_size`
    /// are sync, so we drop the guard before touching the async queue.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build a queue backend under `ENV_LOCK` and release the guard before returning,
    /// so callers can freely `.await` on the queue.
    fn make_queue_for(value: &str) -> Arc<dyn MessageQueue> {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ILINK_MAX_QUEUE_SIZE", value);
        build_queue_backend().unwrap()
    }

    fn make_queue_unset() -> Arc<dyn MessageQueue> {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ILINK_MAX_QUEUE_SIZE");
        build_queue_backend().unwrap()
    }

    /// Cleanup helper — `set_var` is process-global; release the var to keep the
    /// rest of the test suite deterministic.
    fn clear_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ILINK_MAX_QUEUE_SIZE");
    }

    #[tokio::test]
    async fn test_build_queue_backend_max_size_clamp() {
        // Custom valid value: 15
        let q = make_queue_for("15");
        for i in 0..15 {
            let msg = WeixinMessage {
                message_id: Some(i),
                ..Default::default()
            };
            let dropped = q.push("vtoken", msg).await.unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(15),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);
        let drained = q.drain("vtoken").await.unwrap();
        assert_eq!(drained.len(), 15);
        assert_eq!(drained[0].message_id, Some(1));

        // Lower bound clamping: 5 -> clamped to 10
        let q = make_queue_for("5");
        for i in 0..10 {
            let dropped = q
                .push(
                    "vtoken",
                    WeixinMessage {
                        message_id: Some(i),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(10),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);
        let drained = q.drain("vtoken").await.unwrap();
        assert_eq!(drained.len(), 10);

        // Upper bound clamping: 20000 -> clamped to 10000
        let q = make_queue_for("20000");
        for i in 0..10_000 {
            let dropped = q
                .push(
                    "vtoken",
                    WeixinMessage {
                        message_id: Some(i),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(10_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);

        clear_env();
    }

    #[tokio::test]
    async fn test_build_queue_backend_unparseable_falls_back_to_default() {
        // Unparseable value: "abc" -> falls through to default (200)
        let q = make_queue_for("abc");
        // Push up to default and one over to confirm default sizing.
        for i in 0..crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE {
            let dropped = q
                .push(
                    "vtoken",
                    WeixinMessage {
                        message_id: Some(i as i64),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE as i64),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);

        clear_env();
    }

    #[tokio::test]
    async fn test_build_queue_backend_empty_value_falls_back_to_default() {
        // Empty string: "" -> falls through to default (200)
        let q = make_queue_for("");
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!dropped);
        // Confirm we can fill the default-sized queue without dropping.
        for i in 1..crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE {
            let dropped = q
                .push(
                    "vtoken",
                    WeixinMessage {
                        message_id: Some(i as i64),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE as i64),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);

        clear_env();
    }

    #[tokio::test]
    async fn test_build_queue_backend_unset_uses_default() {
        // No env var set -> default (200). This is the common production path.
        let q = make_queue_unset();
        for i in 0..crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE {
            let dropped = q
                .push(
                    "vtoken",
                    WeixinMessage {
                        message_id: Some(i as i64),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(!dropped);
        }
        let dropped = q
            .push(
                "vtoken",
                WeixinMessage {
                    message_id: Some(crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE as i64),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dropped);
    }

    #[test]
    fn test_resolve_max_queue_size_unit() {
        // Pure unit tests for the resolution function — no .await, no async runtime.
        use std::env;

        let _guard = ENV_LOCK.lock().unwrap();

        env::remove_var("ILINK_MAX_QUEUE_SIZE");
        assert_eq!(
            resolve_max_queue_size(),
            crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE
        );

        env::set_var("ILINK_MAX_QUEUE_SIZE", "100");
        assert_eq!(resolve_max_queue_size(), 100);

        env::set_var("ILINK_MAX_QUEUE_SIZE", "5");
        assert_eq!(resolve_max_queue_size(), MIN_MAX_QUEUE_SIZE);

        env::set_var("ILINK_MAX_QUEUE_SIZE", "999999");
        assert_eq!(resolve_max_queue_size(), MAX_MAX_QUEUE_SIZE);

        env::set_var("ILINK_MAX_QUEUE_SIZE", "abc");
        assert_eq!(
            resolve_max_queue_size(),
            crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE
        );

        env::set_var("ILINK_MAX_QUEUE_SIZE", "");
        assert_eq!(
            resolve_max_queue_size(),
            crate::hub::queue::DEFAULT_MAX_QUEUE_SIZE
        );

        // Boundary values that should pass through unclamped.
        env::set_var("ILINK_MAX_QUEUE_SIZE", "10");
        assert_eq!(resolve_max_queue_size(), 10);
        env::set_var("ILINK_MAX_QUEUE_SIZE", "10000");
        assert_eq!(resolve_max_queue_size(), 10_000);

        env::remove_var("ILINK_MAX_QUEUE_SIZE");
    }
}
