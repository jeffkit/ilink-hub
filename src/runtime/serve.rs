//! Start the Axum hub and upstream polling loop until `shutdown` becomes `true`.
//!
//! # Shutdown
//!
//! Pass a [`tokio::sync::watch::Receiver`] whose value the caller sets to `true` when the
//! process should stop (e.g. Ctrl+C in the CLI, or app exit in a desktop shell). The same
//! receiver is cloned for the upstream polling task and for Axum graceful shutdown.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::hub::{
    spawn_dispatcher, spawn_health_checker, spawn_quote_index_evictor, AdminConfig, HubState,
    InMemoryQueue, MessageQueue,
};
use crate::ilink::{LoginClient, QrLoginUiEvent, SessionRenewal, UpstreamClient};
use crate::server::build_router;
use crate::store::Store;

/// Runtime tuning knobs read from environment variables at startup.
///
/// All values are validated eagerly in [`RuntimeConfig::from_env`] so misconfiguration
/// surfaces immediately at launch rather than silently using a wrong default at runtime.
#[derive(Debug)]
pub struct RuntimeConfig {
    /// Capacity of the mpsc channel between the upstream polling loop and the
    /// dispatcher task. When full, the polling loop blocks (backpressure) rather
    /// than dropping messages. Default: 1024.
    pub dispatch_channel_size: usize,
    /// Seconds to wait for in-flight bridge polls to drain before shutdown. Default: 3.
    pub shutdown_drain_secs: u64,
    /// Admin auth config — parsed once here so it can be injected into HubState
    /// and used by routes without re-reading env vars at request time.
    pub admin: AdminConfig,
    /// Hub-wide cap on concurrent `getupdates` long-polls. Defaults to
    /// [`crate::hub::state::MAX_HUB_POLLS_DEFAULT`] (8192). Operators can
    /// raise this via `ILINK_MAX_HUB_POLLS`.
    pub max_hub_polls: usize,
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self> {
        let dispatch_channel_size = parse_env_usize("ILINK_DISPATCH_CHANNEL_SIZE", 1024)?;
        if dispatch_channel_size == 0 {
            anyhow::bail!("ILINK_DISPATCH_CHANNEL_SIZE must be > 0");
        }

        let shutdown_drain_secs =
            parse_env_u64("ILINK_SHUTDOWN_DRAIN_SECS", DEFAULT_SHUTDOWN_DRAIN_SECS)?;

        let max_hub_polls =
            parse_env_usize("ILINK_MAX_HUB_POLLS", crate::hub::MAX_HUB_POLLS_DEFAULT)?;
        if max_hub_polls == 0 {
            anyhow::bail!("ILINK_MAX_HUB_POLLS must be > 0");
        }

        let admin = AdminConfig::from_env();

        if admin.insecure_no_auth
            && std::env::var("ILINK_ADMIN_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some()
        {
            // ILINK_ADMIN_TOKEN takes precedence; the insecure flag is redundant but harmless.
            tracing::warn!(
                "Both ILINK_ADMIN_TOKEN and ILINK_ADMIN_INSECURE_NO_AUTH are set. \
                 ILINK_ADMIN_TOKEN takes effect — the insecure flag is ignored."
            );
        }

        Ok(Self {
            dispatch_channel_size,
            shutdown_drain_secs,
            admin,
            max_hub_polls,
        })
    }

    /// Emit a startup warning if admin auth is disabled, scaled to the actual risk level.
    /// Called after the listen address is known so the message can be actionable.
    pub fn warn_if_insecure(&self, bind_addr: &str) {
        if !self.admin.insecure_no_auth {
            return;
        }
        let is_public = bind_addr.starts_with("0.0.0.0");
        if is_public {
            tracing::error!(
                addr = %bind_addr,
                is_public = true,
                "SECURITY RISK: ILINK_ADMIN_INSECURE_NO_AUTH is set and Hub is bound to \
                 0.0.0.0 (all interfaces). Admin endpoints are accessible with NO authentication. \
                 Set ILINK_ADMIN_TOKEN or restrict the listen address to 127.0.0.1."
            );
        } else {
            tracing::warn!(
                addr = %bind_addr,
                "ILINK_ADMIN_INSECURE_NO_AUTH is set — admin endpoints have no authentication. \
                 Acceptable only on loopback; never expose this port externally."
            );
        }
    }
}

fn parse_env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) if v.trim().is_empty() => Ok(default),
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("{name}={v:?} is not a valid positive integer")),
    }
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) if v.trim().is_empty() => Ok(default),
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{name}={v:?} is not a valid non-negative integer")),
    }
}

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

    // Validate all env-var tuning knobs before doing any async work so that
    // misconfiguration is surfaced immediately at launch.
    let runtime_cfg = RuntimeConfig::from_env()?;
    // Queue backend validation also happens eagerly (it calls build_queue_backend below).

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
    // Load or persist the relay secret BEFORE building HubState. I/O is
    // small (one 32-char file) and synchronous std::fs is acceptable on the
    // startup path; spawning a blocking task for a single file read is more
    // ceremony than it's worth.
    let relay_secret = crate::paths::load_or_create_relay_secret();
    let state = HubState::new(
        upstream.clone() as Arc<dyn crate::ilink::UpstreamSink>,
        store.clone(),
        queue,
        shutdown_rx.clone(),
        relay_secret,
        runtime_cfg.admin.clone(),
    );
    // Apply operator-tuned Hub-wide poll cap. ClientState::new defaults this to
    // MAX_HUB_POLLS_DEFAULT; we override only if the operator set a custom value.
    if runtime_cfg.max_hub_polls != crate::hub::MAX_HUB_POLLS_DEFAULT {
        state
            .clients
            .poll_tracker
            .set_hub_cap(runtime_cfg.max_hub_polls);
    }
    info!(
        max_hub_polls = runtime_cfg.max_hub_polls,
        "hub poll cap installed"
    );

    if let Some(tx) = on_hub_state {
        let _ = tx.send(state.clone());
    }

    load_clients_from_db(state.clone(), store.clone()).await;

    let (tx, rx) =
        mpsc::channel::<crate::ilink::types::WeixinMessage>(runtime_cfg.dispatch_channel_size);

    spawn_dispatcher(state.clone(), rx);

    spawn_health_checker(state.clone());
    spawn_quote_index_evictor(state.clone());

    // Bind the listen socket BEFORE spawning the WeChat polling/login loop.
    // An invalid listen address (e.g. derived from a domain WEIXIN_BASE_URL)
    // must fail fast with a friendly EADDRNOTAVAIL hint deterministically,
    // instead of racing the polling loop's QR-login error output.
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                let is_from_env = std::env::var("WEIXIN_BASE_URL")
                    .ok()
                    .and_then(|val| extract_host_port(&val))
                    .map(|parsed| parsed == addr)
                    .unwrap_or(false);
                if is_from_env {
                    eprintln!(
                        "❌ Failed to bind to address '{}' (EADDRNOTAVAIL).\n\
                         This listen address was derived from WEIXIN_BASE_URL which contains a domain/external URL.\n\
                         To fix this, please specify a local address to listen on, for example:\n\
                         --addr 127.0.0.1:8765 or --addr 0.0.0.0:8765",
                        addr
                    );
                }
            }
            return Err(e.into());
        }
    };
    let local_display = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    if let Some(tx) = on_listening {
        let _ = tx.send(local_display);
    }
    info!(%addr, "iLink Hub listening");
    runtime_cfg.warn_if_insecure(&addr);

    {
        let upstream_clone = upstream.clone();
        let tx_clone = tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();
        let renewal = Some(SessionRenewal {
            store: store.clone(),
            ilink_base_url: ilink_base_url.or(Some(base_url)),
            qr_login_ui,
            qr_tx: Some(state.ilink.qr_tx.clone()),
            qr_last_ready: Some(state.ilink.qr_last_ready.clone()),
            ilink_status: Some(state.ilink.ilink_status.clone()),
            relogin_rx: Some(state.ilink.relogin_tx.subscribe()),
            cached_ui_tx: None,
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
        crate::relay::client::spawn_relay_client(
            identity,
            hub_base,
            relay_ws,
            state.relay_secret.clone(),
            shutdown_rx.clone(),
        );
    } else {
        info!("pairing relay disabled (set HUB_PAIR_URL or ILINKHUB_RELAY=0)");
    }

    let state_for_drain = Arc::clone(&state);
    let router = build_router(state);

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        while !*shutdown_rx.borrow() {
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
        // Wait for pending message queues to drain before closing connections.
        // This gives bridge clients already in-flight time to poll remaining messages.
        drain_queues_before_shutdown(&state_for_drain, runtime_cfg.shutdown_drain_secs).await;
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
            let mut registry = state.clients.registry.write().await;
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
            let mut router = state.routing.router.lock().await;
            let registry = state.clients.registry.read().await;
            for (from_user, vtoken) in routes {
                if registry.get_by_vtoken(&vtoken).is_some() {
                    router.set_route(&from_user, vtoken);
                } else {
                    tracing::warn!(vtoken = %crate::redact_token(&vtoken), from_user = %from_user, "skipping route loading for non-existent vtoken");
                }
            }
            info!(count, "loaded routing state from database");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load routing state from DB");
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

    // Each clamp variant is its own test so env-var mutation never crosses an
    // `.await` point while ENV_LOCK is held. make_queue_for() acquires and
    // releases the lock *before* returning, so the queue is safe to .await.

    #[tokio::test(flavor = "current_thread")]
    async fn test_build_queue_backend_clamp_custom_value() {
        let q = make_queue_for("15");
        for i in 0..15 {
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
        clear_env();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_build_queue_backend_clamp_lower_bound() {
        // 5 -> clamped to MIN (10)
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
        clear_env();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_build_queue_backend_clamp_upper_bound() {
        // 20000 -> clamped to MAX (10000)
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

/// Maximum time to wait for message queues to drain during graceful shutdown (seconds).
///
/// Configurable via `ILINK_SHUTDOWN_DRAIN_SECS`. Set to `0` to disable drain waiting.
/// Default is 30 seconds — enough for most bridge clients to issue a final `getupdates` poll.
const DEFAULT_SHUTDOWN_DRAIN_SECS: u64 = 30;

/// Wait for all per-vtoken message queues to empty before returning, up to `drain_secs`.
///
/// Called during graceful shutdown while the Axum listener is still accepting connections,
/// so bridge clients can continue long-polling and drain the queues normally. Once the queues
/// are empty (or the timeout expires), this returns and Axum closes remaining connections.
///
/// On timeout, logs a warning with the number of undelivered messages so operators can tune
/// the timeout or investigate why bridges are not polling fast enough.
async fn drain_queues_before_shutdown(state: &HubState, drain_secs: u64) {
    if drain_secs == 0 {
        info!("shutdown queue drain disabled (ILINK_SHUTDOWN_DRAIN_SECS=0)");
        return;
    }

    let timeout = Duration::from_secs(drain_secs);
    let check_interval = Duration::from_millis(500);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let sizes = match state.clients.queue.queue_sizes().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to check queue sizes during shutdown drain, proceeding");
                return;
            }
        };

        let total: usize = sizes.values().sum();
        if total == 0 {
            info!("all message queues drained; proceeding with shutdown");
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            warn!(
                pending_messages = total,
                timeout_secs = drain_secs,
                "shutdown drain timeout: {} message(s) undelivered — \
                 increase ILINK_SHUTDOWN_DRAIN_SECS or ensure bridges are online during shutdown",
                total
            );
            return;
        }

        info!(
            pending_messages = total,
            "waiting for message queues to drain before shutdown"
        );
        tokio::time::sleep(check_interval).await;
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::hub::{HubState, InMemoryQueue};
    use crate::ilink::{types::WeixinMessage, UpstreamClient};
    use crate::store::Store;

    async fn make_minimal_state() -> Arc<HubState> {
        let store = Arc::new(Store::connect("sqlite::memory:").await.unwrap());
        let upstream = Arc::new(UpstreamClient::new("sk-test".to_string(), None));
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, rx) = tokio::sync::watch::channel(false);
        HubState::new(
            upstream,
            store,
            queue,
            rx,
            "test-relay-secret".to_string(),
            AdminConfig::from_env(),
        )
    }

    #[tokio::test]
    async fn drain_returns_immediately_when_queues_empty() {
        let state = make_minimal_state().await;
        // No messages pushed — should return without waiting.
        tokio::time::timeout(
            Duration::from_secs(2),
            drain_queues_before_shutdown(&state, 30),
        )
        .await
        .expect("drain_queues_before_shutdown should return immediately for empty queues");
    }

    #[tokio::test]
    async fn drain_times_out_when_queue_not_empty() {
        let state = make_minimal_state().await;

        // Push a message that will never be polled.
        state
            .clients
            .queue
            .push("vt-test", WeixinMessage::default())
            .await
            .unwrap();

        tokio::time::pause();

        // Advance virtual time past the 1-second timeout while drain runs.
        let (_, _) = tokio::join!(drain_queues_before_shutdown(&state, 1), async {
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        // The queue should still have the message (drain timed out, didn't consume it).
        let sizes = state.clients.queue.queue_sizes().await.unwrap();
        assert_eq!(
            sizes.values().sum::<usize>(),
            1,
            "message should remain unpolled after timeout"
        );
    }

    #[tokio::test]
    async fn drain_respects_zero_timeout_disable() {
        let state = make_minimal_state().await;

        state
            .clients
            .queue
            .push("vt-x", WeixinMessage::default())
            .await
            .unwrap();

        // Should return immediately without waiting.
        tokio::time::timeout(
            Duration::from_millis(200),
            drain_queues_before_shutdown(&state, 0),
        )
        .await
        .expect("drain_secs=0 must return immediately");
    }
}

fn extract_host_port(s: &str) -> Option<String> {
    crate::paths::parse_host_port(s)
}
