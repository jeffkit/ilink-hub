//! Bridge manager: discover profile YAML files and supervise one bridge process per file.
//!
//! This is intentionally a thin supervisor. Each child is a normal `ilink-hub-bridge`
//! process, so message handling, Hub registration, session continuity, and CLI execution
//! continue to live in the existing bridge implementation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
extern crate libc;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::bridge::BridgeApp;

const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_RESTART_BACKOFF: Duration = Duration::from_secs(5);
const DEFAULT_MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct BridgeManagerOptions {
    pub hub_url: String,
    pub profiles_dir: PathBuf,
    pub credentials_dir: PathBuf,
    pub scan_interval: Duration,
    pub restart_backoff: Duration,
    pub max_restart_backoff: Duration,
    pub force_register: bool,
    /// Admin token for Hub API calls (e.g. auto-deregister on profile delete).
    /// Parsed once at startup from `ILINK_ADMIN_TOKEN`; callers may override.
    pub admin_token: Option<String>,
}

impl BridgeManagerOptions {
    pub fn new(hub_url: String, profiles_dir: PathBuf, credentials_dir: PathBuf) -> Self {
        let admin_token = std::env::var("ILINK_ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        Self {
            hub_url,
            profiles_dir,
            credentials_dir,
            scan_interval: DEFAULT_SCAN_INTERVAL,
            restart_backoff: DEFAULT_RESTART_BACKOFF,
            max_restart_backoff: DEFAULT_MAX_RESTART_BACKOFF,
            force_register: false,
            admin_token,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BridgeManagerStatus {
    pub state: String,
    pub profiles_total: usize,
    pub running: usize,
    pub restarting: usize,
    pub children: Vec<BridgeChildStatus>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeChildStatus {
    pub id: String,
    pub config_path: String,
    pub register_name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restart_attempts: u32,
    pub next_restart_secs: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct BridgeManagerHandle {
    shutdown: watch::Sender<bool>,
    status: Arc<Mutex<BridgeManagerStatus>>,
}

impl BridgeManagerHandle {
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    pub fn status(&self) -> BridgeManagerStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| BridgeManagerStatus {
                state: "error".into(),
                last_error: Some("bridge manager status lock poisoned".into()),
                ..Default::default()
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    /// mtime of handler script files referenced by the profile (script/args), if any.
    handler_modified: Vec<Option<SystemTime>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeProcessSpec {
    pub id: String,
    pub config_path: PathBuf,
    pub cred_path: PathBuf,
    pub register_name: String,
    fingerprint: FileFingerprint,
}

struct ManagedBridge {
    spec: BridgeProcessSpec,
    child: Option<Child>,
    last_start: Instant,
    restart_attempts: u32,
    state: ManagedBridgeState,
}

enum ManagedBridgeState {
    Running,
    Probing {
        task: JoinHandle<String>,
    },
    Restarting {
        restart_at: Instant,
        last_error: String,
    },
}

/// Run the bridge manager until Ctrl-C.
pub async fn run_bridge_manager(opts: BridgeManagerOptions) -> Result<()> {
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let status = Arc::new(Mutex::new(BridgeManagerStatus::default()));
    run_bridge_manager_with_shutdown(opts, shutdown_rx, status, true).await
}

/// Spawn a bridge manager task and return a handle that can request graceful shutdown.
pub fn spawn_bridge_manager(
    opts: BridgeManagerOptions,
) -> (BridgeManagerHandle, JoinHandle<Result<()>>) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let status = Arc::new(Mutex::new(BridgeManagerStatus::default()));
    let handle = BridgeManagerHandle {
        shutdown: shutdown_tx,
        status: Arc::clone(&status),
    };
    let task = tokio::spawn(run_bridge_manager_with_shutdown(
        opts,
        shutdown_rx,
        status,
        false,
    ));
    (handle, task)
}

/// Run the bridge manager until `shutdown_rx` receives `true`.
pub async fn run_bridge_manager_with_shutdown(
    opts: BridgeManagerOptions,
    mut shutdown_rx: watch::Receiver<bool>,
    status: Arc<Mutex<BridgeManagerStatus>>,
    listen_ctrl_c: bool,
) -> Result<()> {
    tokio::fs::create_dir_all(&opts.profiles_dir)
        .await
        .with_context(|| format!("create profiles dir {}", opts.profiles_dir.display()))?;
    tokio::fs::create_dir_all(&opts.credentials_dir)
        .await
        .with_context(|| format!("create credentials dir {}", opts.credentials_dir.display()))?;

    info!(
        profiles_dir = %opts.profiles_dir.display(),
        credentials_dir = %opts.credentials_dir.display(),
        "bridge manager started"
    );

    let mut manager = BridgeManager::new(opts, status);
    manager.reconcile_once().await?;

    // On Unix, build a SIGTERM future so the manager can stop gracefully when
    // launchd or systemd send SIGTERM.  On non-Unix platforms we use a future
    // that never resolves, keeping the select! arms balanced without cfg guards
    // inside the macro (which tokio::select! does not support).
    let sigterm_fut = make_sigterm_future();

    tokio::pin!(sigterm_fut);

    loop {
        if listen_ctrl_c {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("bridge manager received Ctrl-C; stopping children");
                    manager.stop_all().await;
                    return Ok(());
                }
                _ = &mut sigterm_fut => {
                    info!("bridge manager received SIGTERM; stopping children gracefully");
                    manager.stop_all().await;
                    return Ok(());
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("bridge manager received shutdown request; stopping children");
                        manager.stop_all().await;
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(manager.opts.scan_interval) => {
                    if let Err(e) = manager.reconcile_once().await {
                        manager.set_error(e.to_string());
                        error!(error = %e, "bridge manager reconcile failed");
                    }
                }
            }
        } else {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("bridge manager received shutdown request; stopping children");
                        manager.stop_all().await;
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(manager.opts.scan_interval) => {
                    if let Err(e) = manager.reconcile_once().await {
                        manager.set_error(e.to_string());
                        error!(error = %e, "bridge manager reconcile failed");
                    }
                }
            }
        }
    }
}

struct BridgeManager {
    opts: BridgeManagerOptions,
    status: Arc<Mutex<BridgeManagerStatus>>,
    children: HashMap<String, ManagedBridge>,
    http_client: reqwest::Client,
}

impl BridgeManager {
    fn new(opts: BridgeManagerOptions, status: Arc<Mutex<BridgeManagerStatus>>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest client for BridgeManager");
        Self {
            opts,
            status,
            children: HashMap::new(),
            http_client,
        }
    }

    async fn reconcile_once(&mut self) -> Result<()> {
        let specs = discover_profile_specs(&self.opts.profiles_dir, &self.opts.credentials_dir)
            .await
            .context("discover bridge profile specs")?;
        let desired: HashMap<String, BridgeProcessSpec> =
            specs.into_iter().map(|s| (s.id.clone(), s)).collect();

        self.stop_removed_or_changed(&desired).await;
        self.mark_exited_children().await;
        self.restart_ready_children().await;
        self.start_new_children(desired);
        self.publish_status("running", None);
        Ok(())
    }

    async fn stop_removed_or_changed(&mut self, desired: &HashMap<String, BridgeProcessSpec>) {
        let stale_ids: Vec<String> = self
            .children
            .iter()
            .filter_map(|(id, managed)| match desired.get(id) {
                None => Some(id.clone()),
                Some(spec) if spec.fingerprint != managed.spec.fingerprint => Some(id.clone()),
                _ => None,
            })
            .collect();

        for id in stale_ids {
            if let Some(mut managed) = self.children.remove(&id) {
                info!(profile = %id, "stopping bridge child for removed or changed profile");
                stop_managed_child(&mut managed).await;
                if desired.get(&id).is_none() {
                    // Profile yaml was removed/renamed — clean up orphaned credentials so the
                    // bridge does not re-register under the old name on next manager start.
                    if let Err(e) = tokio::fs::remove_file(&managed.spec.cred_path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            warn!(
                                profile = %id,
                                path = %managed.spec.cred_path.display(),
                                error = %e,
                                "failed to remove orphaned credentials"
                            );
                        }
                    } else {
                        info!(
                            profile = %id,
                            path = %managed.spec.cred_path.display(),
                            "removed orphaned credentials for deleted profile"
                        );
                    }

                    // Auto-deregister the client from Hub so it disappears from /list
                    // immediately instead of lingering as an offline ghost entry.
                    // We use force=true because the child was just killed — it will stop
                    // polling within seconds regardless, and the caller already made the
                    // intent clear by deleting the profile YAML.  Best-effort: if Hub is
                    // unreachable the operator can always clean up manually.
                    let hub_url = self.opts.hub_url.clone();
                    let register_name = managed.spec.register_name.clone();
                    let admin_token = self.opts.admin_token.clone();
                    let http_client = self.http_client.clone();
                    tokio::spawn(async move {
                        match deregister_from_hub(
                            &http_client,
                            &hub_url,
                            &register_name,
                            admin_token.as_deref(),
                        )
                        .await
                        {
                            Ok(()) => {
                                info!(profile = %register_name, "auto-deregistered deleted profile from Hub")
                            }
                            Err(e) => {
                                warn!(profile = %register_name, error = %e, "failed to auto-deregister deleted profile from Hub")
                            }
                        }
                    });
                }
            }
        }
    }

    async fn mark_exited_children(&mut self) {
        let now = Instant::now();
        for (id, managed) in self.children.iter_mut() {
            if !matches!(managed.state, ManagedBridgeState::Running) {
                continue;
            }
            let Some(child) = managed.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!(profile = %id, %status, "bridge child exited");
                    let config_path = managed.spec.config_path.clone();
                    let init_error = format!("bridge child exited: {status}");
                    let task =
                        tokio::spawn(async move { do_probing(config_path, init_error).await });
                    managed.child = None;
                    managed.state = ManagedBridgeState::Probing { task };
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(profile = %id, error = %e, "failed to inspect bridge child");
                    let config_path = managed.spec.config_path.clone();
                    let init_error = format!("failed to inspect bridge child: {e}");
                    let task =
                        tokio::spawn(async move { do_probing(config_path, init_error).await });
                    managed.child = None;
                    managed.state = ManagedBridgeState::Probing { task };
                }
            }
        }

        let mut finished_probes = Vec::new();
        for (id, managed) in self.children.iter_mut() {
            if let ManagedBridgeState::Probing { task } = &managed.state {
                if task.is_finished() {
                    finished_probes.push(id.clone());
                }
            }
        }

        for id in finished_probes {
            if let Some(managed) = self.children.get_mut(&id) {
                let old_state = std::mem::replace(&mut managed.state, ManagedBridgeState::Running);
                if let ManagedBridgeState::Probing { task } = old_state {
                    let error = match task.await {
                        Ok(err) => err,
                        Err(join_err) => format!("probing task panicked: {join_err}"),
                    };
                    managed.restart_attempts = next_restart_attempts(
                        managed.restart_attempts,
                        now.duration_since(managed.last_start),
                        self.opts.max_restart_backoff.saturating_mul(3),
                    );
                    let delay = restart_delay(
                        self.opts.restart_backoff,
                        self.opts.max_restart_backoff,
                        managed.restart_attempts,
                    );
                    managed.state = ManagedBridgeState::Restarting {
                        restart_at: now + delay,
                        last_error: error,
                    };
                }
            }
        }
    }
}

async fn do_probing(config_path: PathBuf, mut error: String) -> String {
    if let Ok(app) = BridgeApp::load(&config_path) {
        if let Some(profile) = app.profile(app.default_profile_name()) {
            match crate::bridge::probe_profile_light(profile) {
                Err(e) => {
                    error = e.to_string();
                }
                Ok(()) => {
                    if let Err(e) = crate::bridge::dry_run_profile(profile, "ping").await {
                        error = e.to_string();
                    }
                }
            }
        }
    }
    error
}

impl BridgeManager {
    async fn restart_ready_children(&mut self) {
        let now = Instant::now();
        let ready_ids: Vec<String> = self
            .children
            .iter()
            .filter_map(|(id, managed)| match managed.state {
                ManagedBridgeState::Restarting { restart_at, .. } if now >= restart_at => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();

        for id in ready_ids {
            if let Some(mut managed) = self.children.remove(&id) {
                stop_managed_child(&mut managed).await;
                let spec = managed.spec.clone();
                match spawn_bridge_child(&self.opts, &spec) {
                    Ok(child) => {
                        info!(profile = %id, "restarted bridge child");
                        managed.child = Some(child);
                        managed.last_start = Instant::now();
                        managed.state = ManagedBridgeState::Running;
                        self.children.insert(id, managed);
                    }
                    Err(e) => {
                        let error = format!("failed to restart bridge child: {e}");
                        error!(profile = %id, error = %e, "failed to restart bridge child");
                        managed.restart_attempts = managed.restart_attempts.saturating_add(1);
                        let delay = restart_delay(
                            self.opts.restart_backoff,
                            self.opts.max_restart_backoff,
                            managed.restart_attempts,
                        );
                        managed.state = ManagedBridgeState::Restarting {
                            restart_at: Instant::now() + delay,
                            last_error: error,
                        };
                        self.children.insert(id, managed);
                    }
                }
            }
        }
    }

    fn start_new_children(&mut self, desired: HashMap<String, BridgeProcessSpec>) {
        for (id, spec) in desired {
            if self.children.contains_key(&id) {
                continue;
            }

            let mut probe_error = None;
            if let Ok(app) = BridgeApp::load(&spec.config_path) {
                if let Some(profile) = app.profile(app.default_profile_name()) {
                    if let Err(e) = crate::bridge::probe_profile_light(profile) {
                        probe_error = Some(e.to_string());
                    }
                }
            }

            if let Some(err) = probe_error {
                error!(profile = %id, error = %err, "failed to start bridge child: probe failed");
                let delay =
                    restart_delay(self.opts.restart_backoff, self.opts.max_restart_backoff, 1);
                self.children.insert(
                    id.clone(),
                    ManagedBridge {
                        spec,
                        child: None,
                        last_start: Instant::now(),
                        restart_attempts: 1,
                        state: ManagedBridgeState::Restarting {
                            restart_at: Instant::now() + delay,
                            last_error: err,
                        },
                    },
                );
                continue;
            }

            match spawn_bridge_child(&self.opts, &spec) {
                Ok(child) => {
                    info!(
                        profile = %id,
                        config = %spec.config_path.display(),
                        register_name = %spec.register_name,
                        "started bridge child"
                    );
                    self.children.insert(
                        id,
                        ManagedBridge {
                            spec,
                            child: Some(child),
                            last_start: Instant::now(),
                            restart_attempts: 0,
                            state: ManagedBridgeState::Running,
                        },
                    );
                }
                Err(e) => {
                    error!(profile = %id, error = %e, "failed to start bridge child");
                    let delay =
                        restart_delay(self.opts.restart_backoff, self.opts.max_restart_backoff, 1);
                    self.children.insert(
                        id,
                        ManagedBridge {
                            spec,
                            child: None,
                            last_start: Instant::now(),
                            restart_attempts: 1,
                            state: ManagedBridgeState::Restarting {
                                restart_at: Instant::now() + delay,
                                last_error: format!("failed to start bridge child: {e}"),
                            },
                        },
                    );
                }
            }
        }
    }

    async fn stop_all(&mut self) {
        // Send SIGTERM to all children concurrently then wait concurrently.
        // Sequential stop_managed_child accumulates up to 5s × N total wait time.
        let managed_list: Vec<ManagedBridge> = self.children.drain().map(|(_, m)| m).collect();

        // Abort probing tasks and collect child processes.
        let mut children: Vec<Child> = Vec::new();
        for managed in managed_list {
            if let ManagedBridgeState::Probing { task } = &managed.state {
                task.abort();
            }
            if let Some(child) = managed.child {
                children.push(child);
            }
        }

        // Phase 1: SIGTERM all running children in parallel (non-blocking syscall).
        #[cfg(unix)]
        for child in &children {
            if let Some(pid) = child.id() {
                // SAFETY: `pid` is the OS-assigned pid of a `tokio::process::Child`
                // we own — we spawned it, we never reaped it, and we are the
                // only writer. PIDs are not reused within the brief window
                // between signal delivery and `wait()` since the same kernel
                // holds the live task struct. `kill` is a pure syscall with
                // no memory-safety surface. The call is racing against the
                // child exiting on its own, which is harmless: an
                // `ESRCH` (no such process) is the expected outcome and is
                // already handled below.
                let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                if rc != 0 {
                    let errno = std::io::Error::last_os_error();
                    warn!(pid, error = %errno, "failed to send SIGTERM during stop_all");
                }
            }
        }

        // Phase 2: spawn a task per child to wait concurrently (5s grace, then SIGKILL).
        const GRACEFUL: u64 = 5;
        let handles: Vec<JoinHandle<()>> = children
            .into_iter()
            .map(|mut child| {
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(GRACEFUL), child.wait()).await {
                        Ok(Ok(_)) => {}
                        _ => {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            let _ = handle.await;
        }

        self.publish_status("stopped", None);
    }

    fn set_error(&self, error: String) {
        self.publish_status("error", Some(error));
    }

    fn publish_status(&self, state: &str, last_error: Option<String>) {
        let now = Instant::now();
        let mut children = self
            .children
            .values()
            .map(|managed| {
                let (state, next_restart_secs, last_error, pid, uptime_secs) = match &managed.state
                {
                    ManagedBridgeState::Running => (
                        "running".to_string(),
                        None,
                        None,
                        managed.child.as_ref().and_then(|child| child.id()),
                        Some(now.duration_since(managed.last_start).as_secs()),
                    ),
                    ManagedBridgeState::Probing { .. } => {
                        ("probing".to_string(), None, None, None, None)
                    }
                    ManagedBridgeState::Restarting {
                        restart_at,
                        last_error,
                    } => (
                        "restarting".to_string(),
                        Some(restart_at.saturating_duration_since(now).as_secs()),
                        Some(last_error.clone()),
                        None,
                        None,
                    ),
                };
                BridgeChildStatus {
                    id: managed.spec.id.clone(),
                    config_path: managed.spec.config_path.display().to_string(),
                    register_name: managed.spec.register_name.clone(),
                    state,
                    pid,
                    uptime_secs,
                    restart_attempts: managed.restart_attempts,
                    next_restart_secs,
                    last_error,
                }
            })
            .collect::<Vec<_>>();
        children.sort_by(|a, b| a.id.cmp(&b.id));
        let running = children.iter().filter(|c| c.state == "running").count();
        let restarting = children.iter().filter(|c| c.state == "restarting").count();

        if let Ok(mut status) = self.status.lock() {
            *status = BridgeManagerStatus {
                state: state.to_string(),
                profiles_total: children.len(),
                running,
                restarting,
                children,
                last_error,
            };
        }
    }
}

async fn stop_managed_child(managed: &mut ManagedBridge) {
    if let Some(child) = managed.child.as_mut() {
        stop_child(child).await;
    }
    managed.child = None;
    if let ManagedBridgeState::Probing { task } = &managed.state {
        task.abort();
    }
}

fn restart_delay(base: Duration, max: Duration, attempts: u32) -> Duration {
    let base_secs = base.as_secs().max(1);
    let max_secs = max.as_secs().max(base_secs);
    let shift = attempts.saturating_sub(1).min(20);
    let multiplier = 1u64 << shift;
    Duration::from_secs(base_secs.saturating_mul(multiplier).min(max_secs))
}

fn next_restart_attempts(previous: u32, uptime: Duration, reset_after: Duration) -> u32 {
    let previous = if uptime >= reset_after { 0 } else { previous };
    previous.saturating_add(1)
}

async fn stop_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            warn!(error = %e, "failed to inspect bridge child before stop");
        }
    }

    // Send SIGTERM first so the child bridge can cancel in-flight AI calls and send
    // error replies to users before exiting.  Fall back to SIGKILL after a grace period.
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: see the symmetric note in `stop_removed_profile`: the
            // `pid` comes from a child we own, `kill` is a pure syscall, and
            // any `ESRCH` is the expected "child already exited" case
            // handled below.
            let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            if rc != 0 {
                let errno = std::io::Error::last_os_error();
                warn!(pid, error = %errno, "failed to send SIGTERM to bridge child");
            } else {
                // Give the child up to GRACEFUL_SHUTDOWN_SECS to finish its error replies.
                const GRACEFUL_SHUTDOWN_SECS: u64 = 5;
                match tokio::time::timeout(
                    Duration::from_secs(GRACEFUL_SHUTDOWN_SECS),
                    child.wait(),
                )
                .await
                {
                    Ok(Ok(_)) => return, // exited cleanly within the grace period
                    Ok(Err(e)) => warn!(error = %e, "error waiting for bridge child after SIGTERM"),
                    Err(_) => info!(
                        pid,
                        "bridge child did not exit after SIGTERM; sending SIGKILL"
                    ),
                }
            }
        }
    }

    if let Err(e) = child.kill().await {
        warn!(error = %e, "failed to kill bridge child");
    }
    let _ = child.wait().await;
}

async fn discover_profile_specs(
    profiles_dir: &Path,
    credentials_dir: &Path,
) -> Result<Vec<BridgeProcessSpec>> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(profiles_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", profiles_dir.display())),
    };

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "skip unreadable bridge profile");
                continue;
            }
        };
        if !metadata.is_file() || !is_yaml_file(&path) {
            continue;
        }
        if let Err(e) = BridgeApp::load(&path) {
            warn!(path = %path.display(), error = %e, "skip invalid bridge profile YAML");
            continue;
        }
        out.push(spec_from_profile_file(path, metadata, credentials_dir));
    }

    out.sort_by(|a, b| a.config_path.cmp(&b.config_path));
    disambiguate_duplicate_ids(&mut out, credentials_dir);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn disambiguate_duplicate_ids(specs: &mut [BridgeProcessSpec], credentials_dir: &Path) {
    let mut seen = HashSet::new();
    for spec in specs {
        if seen.insert(spec.id.clone()) {
            continue;
        }

        let original = spec.id.clone();
        let unique = unique_profile_id(&original, &spec.config_path, &seen);
        warn!(
            profile_id = %original,
            unique_profile_id = %unique,
            path = %spec.config_path.display(),
            "bridge profile id collision; using deterministic suffix"
        );
        spec.id = unique.clone();
        spec.register_name = unique.clone();
        spec.cred_path = credentials_dir.join(format!("{unique}.json"));
        seen.insert(unique);
    }
}

/// FNV-1a 32-bit hash — stable across Rust versions, no external dependencies.
fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 2166136261;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn unique_profile_id(base: &str, path: &Path, seen: &HashSet<String>) -> String {
    // FNV-1a 32-bit: no external dependency, stable across Rust versions and platforms.
    let hash = format!("{:08x}", fnv1a32(path.to_string_lossy().as_bytes()));
    let max_base = 64usize.saturating_sub(hash.len() + 1);
    let mut prefix = base.chars().take(max_base).collect::<String>();
    if prefix.is_empty() {
        prefix = "profile".to_string();
    }
    let mut candidate = format!("{prefix}-{hash}");
    let mut n = 2u32;
    while seen.contains(&candidate) {
        candidate = format!("{prefix}-{hash}-{n}");
        n += 1;
    }
    candidate
}

fn spec_from_profile_file(
    config_path: PathBuf,
    metadata: std::fs::Metadata,
    credentials_dir: &Path,
) -> BridgeProcessSpec {
    let id = profile_id_from_path(&config_path);
    let cred_path = credentials_dir.join(format!("{id}.json"));
    let handler_modified = handler_mtimes(&config_path);
    BridgeProcessSpec {
        id: id.clone(),
        config_path,
        cred_path,
        register_name: id,
        fingerprint: FileFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            handler_modified,
        },
    }
}

/// Collect mtimes of local handler files referenced by the profile YAML.
///
/// Looks at `script` and the first element of `args` for each profile, resolving
/// relative paths against the yaml file's directory. Non-existent or non-local
/// paths are silently skipped.
fn handler_mtimes(config_path: &Path) -> Vec<Option<SystemTime>> {
    let Ok(app) = crate::bridge::BridgeApp::load(config_path) else {
        return vec![];
    };
    let base = config_path.parent().unwrap_or(Path::new("."));
    let mut mtimes = Vec::new();
    for name in app.profile_names() {
        let Some(profile) = app.profile(name) else {
            continue;
        };
        // Candidates: the script path (before expansion) or args[0] (the handler file).
        let candidates: Vec<&str> = profile
            .args
            .first()
            .map(|s| s.as_str())
            .into_iter()
            .collect();
        for candidate in candidates {
            let path = Path::new(candidate);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            };
            match std::fs::metadata(&resolved) {
                Ok(m) if m.is_file() => mtimes.push(m.modified().ok()),
                _ => {}
            }
        }
    }
    mtimes.sort_by_key(|t| t.map(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok()));
    mtimes
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
    )
}

fn profile_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("profile");
    sanitize_profile_id(stem)
}

fn sanitize_profile_id(raw: &str) -> String {
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
        "profile".to_string()
    } else {
        out
    }
}

fn bridge_child_args(opts: &BridgeManagerOptions, spec: &BridgeProcessSpec) -> Vec<String> {
    let mut args = vec![
        "--hub-url".to_string(),
        opts.hub_url.clone(),
        "--cred-file".to_string(),
        spec.cred_path.to_string_lossy().into_owned(),
        "--register-name".to_string(),
        spec.register_name.clone(),
        "--config".to_string(),
        spec.config_path.to_string_lossy().into_owned(),
    ];
    if opts.force_register {
        args.push("--force-register".to_string());
    }
    args
}

fn spawn_bridge_child(opts: &BridgeManagerOptions, spec: &BridgeProcessSpec) -> Result<Child> {
    let exe = super::resolve_bridge_executable();
    let mut cmd = Command::new(&exe);
    cmd.args(bridge_child_args(opts, spec));

    // Manager children must get independent identities derived from their profile file.
    // Remove env vars that would otherwise force all children to share one token/credential path.
    cmd.env_remove("WEIXIN_TOKEN");
    cmd.env_remove("ILINKHUB_BRIDGE_CREDS");
    cmd.env_remove("ILINKHUB_BRIDGE_REGISTER_NAME");
    // Explicitly forward the admin token (also inherited by default) so each child can
    // auto-register independently against an auth-protected Hub. Without it, registration
    // returns 401 and tempts operators to reuse an existing backend's token, which causes
    // multiple bridges to share one vtoken / message queue.
    if let Ok(admin) = std::env::var("ILINK_ADMIN_TOKEN") {
        if !admin.trim().is_empty() {
            cmd.env("ILINK_ADMIN_TOKEN", admin);
        }
    }
    cmd.kill_on_drop(true);

    cmd.spawn().context("spawn bridge child")
}

/// Call `DELETE /hub/clients/{name}?force=true` on the Hub to remove an offline bridge.
///
/// Uses `force=true` so the request succeeds even when the Hub has not yet had time to
/// run its health-check and flip the client to `online=false` after the child was killed.
///
/// Returns `Ok(())` when the client was deleted or was already absent (404).
async fn deregister_from_hub(
    client: &reqwest::Client,
    hub_url: &str,
    name: &str,
    admin_token: Option<&str>,
) -> anyhow::Result<()> {
    let url = format!(
        "{}/hub/clients/{}?force=true",
        hub_url.trim_end_matches('/'),
        name
    );
    let mut req = client.delete(&url);
    if let Some(token) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", token.trim()));
    }
    let resp = req.send().await.context("DELETE /hub/clients")?;
    match resp.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::NOT_FOUND => Ok(()), // already gone
        s => {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Hub returned {s}: {body}");
        }
    }
}

/// Returns a future that resolves when SIGTERM is received on Unix, or never on
/// other platforms.  Used in `tokio::select!` to avoid `#[cfg]` inside macros.
async fn make_sigterm_future() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ilink-hub-bridge-manager-test-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_profile(path: &Path) {
        fs::write(
            path,
            r#"
command: echo
args: ["ok"]
stdin: none
"#,
        )
        .unwrap();
    }

    #[test]
    fn sanitize_profile_id_keeps_workspace_safe() {
        assert_eq!(sanitize_profile_id("Claude Project A"), "Claude-Project-A");
        assert_eq!(sanitize_profile_id("  你好 / demo  "), "demo");
        assert_eq!(sanitize_profile_id("!!!"), "profile");
    }

    #[test]
    fn yaml_file_detection_accepts_yaml_and_yml() {
        assert!(is_yaml_file(Path::new("a.yaml")));
        assert!(is_yaml_file(Path::new("a.YML")));
        assert!(!is_yaml_file(Path::new("a.json")));
    }

    #[tokio::test]
    async fn discover_specs_uses_existing_yaml_and_independent_credentials() {
        let profiles = temp_dir("profiles");
        let creds = temp_dir("creds");
        write_profile(&profiles.join("claude project.yaml"));
        write_profile(&profiles.join("codex.yml"));
        fs::write(profiles.join("notes.txt"), "ignore").unwrap();

        let specs = discover_profile_specs(&profiles, &creds).await.unwrap();
        let ids: Vec<_> = specs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-project", "codex"]);
        assert_eq!(specs[0].register_name, "claude-project");
        assert_eq!(specs[0].cred_path, creds.join("claude-project.json"));
        assert_eq!(specs[1].cred_path, creds.join("codex.json"));
    }

    #[tokio::test]
    async fn stop_removed_profile_cleans_up_credentials() {
        let profiles = temp_dir("cleanup-profiles");
        let creds = temp_dir("cleanup-creds");

        // Write a profile and a matching credentials file (simulating a registered bridge).
        write_profile(&profiles.join("old-profile.yaml"));
        let cred_file = creds.join("old-profile.json");
        fs::write(&cred_file, r#"{"token":"fake"}"#).unwrap();

        // Desired has no "old-profile" (simulates yaml being removed/renamed).
        let desired: HashMap<String, BridgeProcessSpec> = HashMap::new();

        let fingerprint = FileFingerprint {
            len: 0,
            modified: None,
            handler_modified: vec![],
        };
        let spec = BridgeProcessSpec {
            id: "old-profile".into(),
            config_path: profiles.join("old-profile.yaml"),
            cred_path: cred_file.clone(),
            register_name: "old-profile".into(),
            fingerprint,
        };
        let mut manager = BridgeManager::new(
            BridgeManagerOptions::new(
                "http://127.0.0.1:8765".into(),
                profiles.clone(),
                creds.clone(),
            ),
            Arc::new(Mutex::new(BridgeManagerStatus::default())),
        );
        manager.children.insert(
            "old-profile".into(),
            ManagedBridge {
                spec,
                child: None,
                last_start: Instant::now(),
                restart_attempts: 0,
                state: ManagedBridgeState::Running,
            },
        );

        manager.stop_removed_or_changed(&desired).await;

        assert!(
            !cred_file.exists(),
            "orphaned credentials should be removed"
        );
    }

    #[tokio::test]
    async fn discover_specs_disambiguates_sanitized_id_collisions() {
        let profiles = temp_dir("collisions");
        let creds = temp_dir("collision-creds");
        write_profile(&profiles.join("demo profile.yaml"));
        write_profile(&profiles.join("demo-profile.yml"));

        let specs = discover_profile_specs(&profiles, &creds).await.unwrap();
        assert_eq!(specs.len(), 2);
        assert_ne!(specs[0].id, specs[1].id);
        assert!(specs.iter().any(|s| s.id == "demo-profile"));
        assert!(specs.iter().any(|s| s.id.starts_with("demo-profile-")));
        for spec in specs {
            assert_eq!(spec.register_name, spec.id);
            assert_eq!(spec.cred_path, creds.join(format!("{}.json", spec.id)));
        }
    }

    #[tokio::test]
    async fn discover_specs_skips_invalid_yaml() {
        let profiles = temp_dir("invalid");
        let creds = temp_dir("invalid-creds");
        fs::write(profiles.join("bad.yaml"), "command:").unwrap();

        let specs = discover_profile_specs(&profiles, &creds).await.unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn child_args_pin_identity_per_profile() {
        let opts = BridgeManagerOptions {
            hub_url: "http://127.0.0.1:8765".into(),
            profiles_dir: PathBuf::from("/profiles"),
            credentials_dir: PathBuf::from("/creds"),
            scan_interval: Duration::from_secs(1),
            restart_backoff: Duration::from_secs(1),
            max_restart_backoff: Duration::from_secs(60),
            force_register: true,
            admin_token: None,
        };
        let spec = BridgeProcessSpec {
            id: "claude".into(),
            config_path: PathBuf::from("/profiles/claude.yaml"),
            cred_path: PathBuf::from("/creds/claude.json"),
            register_name: "claude".into(),
            fingerprint: FileFingerprint {
                len: 1,
                modified: None,
                handler_modified: vec![],
            },
        };

        assert_eq!(
            bridge_child_args(&opts, &spec),
            vec![
                "--hub-url",
                "http://127.0.0.1:8765",
                "--cred-file",
                "/creds/claude.json",
                "--register-name",
                "claude",
                "--config",
                "/profiles/claude.yaml",
                "--force-register",
            ]
        );
    }

    #[test]
    fn restart_delay_grows_exponentially_with_cap() {
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(30);
        assert_eq!(restart_delay(base, max, 0), Duration::from_secs(5));
        assert_eq!(restart_delay(base, max, 1), Duration::from_secs(5));
        assert_eq!(restart_delay(base, max, 2), Duration::from_secs(10));
        assert_eq!(restart_delay(base, max, 3), Duration::from_secs(20));
        assert_eq!(restart_delay(base, max, 4), Duration::from_secs(30));
        assert_eq!(restart_delay(base, max, 10), Duration::from_secs(30));
    }

    #[test]
    fn restart_attempts_reset_after_healthy_uptime() {
        let reset_after = Duration::from_secs(60);
        assert_eq!(
            next_restart_attempts(3, Duration::from_secs(10), reset_after),
            4
        );
        assert_eq!(
            next_restart_attempts(3, Duration::from_secs(60), reset_after),
            1
        );
        assert_eq!(
            next_restart_attempts(3, Duration::from_secs(120), reset_after),
            1
        );
    }

    #[tokio::test]
    async fn spawned_manager_stops_via_handle() {
        let profiles = temp_dir("handle-profiles");
        let creds = temp_dir("handle-creds");
        let mut opts = BridgeManagerOptions::new(
            "http://127.0.0.1:8765".into(),
            profiles.clone(),
            creds.clone(),
        );
        opts.scan_interval = Duration::from_millis(25);
        opts.restart_backoff = Duration::from_millis(25);
        opts.max_restart_backoff = Duration::from_millis(100);

        let (handle, task) = spawn_bridge_manager(opts);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status = handle.status();
        assert_eq!(status.profiles_total, 0);

        handle.stop();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("manager task should stop")
            .expect("manager task join");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn spawned_manager_stops_when_handle_is_dropped() {
        let profiles = temp_dir("drop-handle-profiles");
        let creds = temp_dir("drop-handle-creds");
        let mut opts = BridgeManagerOptions::new("http://127.0.0.1:8765".into(), profiles, creds);
        opts.scan_interval = Duration::from_millis(25);

        let (handle, task) = spawn_bridge_manager(opts);
        drop(handle);

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("manager task should stop when sender is dropped")
            .expect("manager task join");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mark_exited_children_does_not_block_and_probes() {
        let temp_dir = temp_dir("probing-test");
        let profile_path = temp_dir.join("test-profile.yaml");
        fs::write(
            &profile_path,
            r#"
command: sleep
args: ["1"]
"#,
        )
        .unwrap();

        let spec = BridgeProcessSpec {
            id: "test-profile".into(),
            config_path: profile_path,
            cred_path: temp_dir.join("test-profile.json"),
            register_name: "test-profile".into(),
            fingerprint: FileFingerprint {
                len: 0,
                modified: None,
                handler_modified: vec![],
            },
        };

        let mut manager = BridgeManager::new(
            BridgeManagerOptions::new(
                "http://127.0.0.1:8765".into(),
                temp_dir.clone(),
                temp_dir.clone(),
            ),
            Arc::new(Mutex::new(BridgeManagerStatus::default())),
        );

        // Spawn a child process that exits immediately
        let mut cmd = tokio::process::Command::new("true");
        let mut child = cmd.spawn().unwrap();

        // Wait until the child process has actually exited
        while child.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        manager.children.insert(
            "test-profile".into(),
            ManagedBridge {
                spec,
                child: Some(child),
                last_start: Instant::now(),
                restart_attempts: 0,
                state: ManagedBridgeState::Running,
            },
        );

        // Call mark_exited_children. It should transition the child to Probing immediately without blocking.
        let start = Instant::now();
        manager.mark_exited_children().await;
        let elapsed = start.elapsed();

        // Assert that it took less than 500ms (probing sleep 1 in background shouldn't block us)
        assert!(
            elapsed < Duration::from_millis(500),
            "mark_exited_children blocked the scheduler!"
        );

        // The state should now be Probing
        let managed = manager.children.get("test-profile").unwrap();
        assert!(matches!(managed.state, ManagedBridgeState::Probing { .. }));

        // Wait for the background probing task to finish
        let mut attempts = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            manager.mark_exited_children().await;
            let managed = manager.children.get("test-profile").unwrap();
            if matches!(managed.state, ManagedBridgeState::Restarting { .. }) {
                break;
            }
            attempts += 1;
            if attempts > 40 {
                panic!("Task did not transition to Restarting after probing");
            }
        }

        // Verify that the state is now Restarting
        let managed = manager.children.get("test-profile").unwrap();
        if let ManagedBridgeState::Restarting { last_error, .. } = &managed.state {
            assert!(
                last_error.contains("exited") || last_error.is_empty(),
                "unexpected error: {}",
                last_error
            );
        } else {
            panic!("expected Restarting state");
        }
    }
}
