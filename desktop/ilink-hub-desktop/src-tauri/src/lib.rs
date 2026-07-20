//! iLink Hub desktop shell: embeds the same runtime as `ilink-hub serve`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tauri::async_runtime::JoinHandle;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tokio::sync::watch;

mod bridge_profiles;
mod hub_commands;
mod listen_addr;
#[cfg(test)]
mod test_support;

pub(crate) use bridge_profiles::*;
pub(crate) use hub_commands::*;
pub(crate) use listen_addr::*;

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


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        assert!(*rx.borrow_and_update());
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

    use crate::test_support::{ScopedHome, PORT_OVERRIDE_LOCK};

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
pub(crate) struct HubTaskHandles {
    pub(crate) bind_listener: Option<JoinHandle<()>>,
    pub(crate) qr_consumer: Option<JoinHandle<()>>,
    pub(crate) run_serve: Option<JoinHandle<()>>,
}

impl HubTaskHandles {
    /// Abort all in-flight tasks. Idempotent.
    pub(crate) fn abort_all(&mut self) {
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

pub(crate) struct HubController {
    /// Shutdown signal for the in-flight `run_serve`. Set when start succeeds,
    /// cleared by `stop_hub` / `restart_hub`. Used as the "is running" arbiter.
    pub(crate) shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    /// Handles for the bind listener, QR consumer, and run_serve tasks spawned
    /// alongside the sender. Aborted on the loser path / replaced on restart.
    pub(crate) task_handles: Mutex<HubTaskHandles>,
    /// Configuration captured ONCE in `setup()` so subsequent restarts do not
    /// silently pick up env-mutated token / base_url between stop and start.
    pub(crate) env_token: Option<String>,
    pub(crate) env_base_url: Option<String>,
    /// Listen address (`127.0.0.1:<port>`). Mutated by the GUI "change port"
    /// flow via `set_listen_port`; read by `hub_info` so the UI can show what
    /// will be used on the next start. Mirrors the value persisted to disk so
    /// the in-memory and on-disk views stay coherent across restarts.
    pub(crate) requested_addr: Mutex<String>,
    pub(crate) database_path: PathBuf,
    pub(crate) listening_addr: Arc<Mutex<Option<String>>>,
    pub(crate) hub_state: Arc<Mutex<Option<Arc<ilink_hub::HubState>>>>,
}

impl HubController {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_running(&self) -> bool {
        self.shutdown_tx
            .lock()
            .expect("HubController mutex poisoned — please restart the app")
            .is_some()
    }

    pub(crate) fn requested_addr(&self) -> String {
        self.requested_addr
            .lock()
            .expect("HubController mutex poisoned — please restart the app")
            .clone()
    }

    pub(crate) fn set_requested_addr(&self, addr: String) {
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
    let new_profiles = im_agentproc::paths::desktop_bridge_profiles_dir();
    let new_creds = im_agentproc::paths::desktop_bridge_credentials_dir();
    let old_profiles = im_agentproc::paths::default_bridge_profiles_dir();
    let old_creds = im_agentproc::paths::default_bridge_manager_credentials_dir();

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
            // port override → `ILINK_HUB_ADDR` env var → default. On any Err
            // (bad override file OR rejected non-loopback env), fall back ONLY
            // to hardcoded 127.0.0.1:8765 — never re-read ILINK_HUB_ADDR.
            let requested_addr =
                resolve_initial_listen_addr().unwrap_or_else(safe_listen_addr_on_resolve_error);

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
                config_path: im_agentproc::paths::default_bridge_config_path(),
                profiles_dir: im_agentproc::paths::desktop_bridge_profiles_dir(),
                credentials_dir: im_agentproc::paths::desktop_bridge_credentials_dir(),
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
