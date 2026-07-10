//! Loopback listen-address helpers and desktop port override persistence.

use std::path::PathBuf;

use tauri::Manager;

use crate::HubController;

pub(crate) fn loopback_hub_origin(listen_addr: &str) -> String {
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
pub(crate) fn desktop_port_override_path() -> PathBuf {
    ilink_hub::paths::data_dir().join("desktop-port.json")
}

/// Compose the loopback listen address `127.0.0.1:<port>` for a user-selected
/// port. Centralised so tests and the command handler agree on the exact form.
pub(crate) fn loopback_listen_addr_for_port(port: u16) -> String {
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
pub(crate) fn load_desktop_port_override() -> Result<Option<u16>, String> {
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
pub(crate) fn save_desktop_port_override(port: u16) -> Result<(), String> {
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

/// Hardcoded safe default listen address. Used whenever resolve fails.
///
/// CRITICAL: setup must never re-read `ILINK_HUB_ADDR` on Err — that env value
/// may be the non-loopback address that caused the rejection (e.g. `0.0.0.0:8765`).
pub(crate) const SAFE_LOOPBACK_LISTEN_ADDR: &str = "127.0.0.1:8765";

/// Resolve the listen address the desktop shell should use on first start.
///
/// Priority: persisted port override → `ILINK_HUB_ADDR` env var → default
/// `127.0.0.1:8765`. The port override only overrides the port; the host
/// stays loopback so the saved choice cannot accidentally rebind on a
/// non-loopback interface.
///
/// When falling back to `ILINK_HUB_ADDR`, only loopback hosts are accepted
/// (`127.0.0.1`, `localhost`, `::1`, or a bare port which implies loopback).
/// Non-loopback values such as `0.0.0.0` or LAN IPs are rejected.
pub(crate) fn resolve_initial_listen_addr() -> Result<String, String> {
    match load_desktop_port_override()? {
        Some(port) => Ok(loopback_listen_addr_for_port(port)),
        None => match std::env::var("ILINK_HUB_ADDR") {
            Ok(addr) => {
                ensure_loopback_listen_addr(&addr)?;
                Ok(addr)
            }
            Err(_) => Ok(SAFE_LOOPBACK_LISTEN_ADDR.into()),
        },
    }
}

/// Fallback used by `setup()` when [`resolve_initial_listen_addr`] returns Err.
///
/// Always returns the hardcoded loopback default. Never re-reads
/// `ILINK_HUB_ADDR` — that would undo the loopback check (M2 f1).
pub(crate) fn safe_listen_addr_on_resolve_error(err: impl std::fmt::Display) -> String {
    tracing::warn!(
        error = %err,
        fallback = SAFE_LOOPBACK_LISTEN_ADDR,
        "resolve_initial_listen_addr failed; using hardcoded loopback default \
         (never re-read ILINK_HUB_ADDR)"
    );
    SAFE_LOOPBACK_LISTEN_ADDR.into()
}

/// Return `Ok(())` when `addr` binds only on a loopback host; otherwise Err.
///
/// Accepted forms:
/// - `127.0.0.1:<port>`
/// - `localhost:<port>`
/// - `[::1]:<port>` / `::1:<port>`
/// - bare `<port>` (treated as loopback by the rest of the desktop stack)
pub(crate) fn ensure_loopback_listen_addr(addr: &str) -> Result<(), String> {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return Err("ILINK_HUB_ADDR is empty".into());
    }

    // Bare port → loopback implied.
    if !trimmed.contains(':') {
        return trimmed
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .map(|_| ())
            .ok_or_else(|| format!("ILINK_HUB_ADDR `{trimmed}` is not a valid port"));
    }

    // Strip optional scheme so `http://127.0.0.1:8765` still works if pasted.
    let s = trimmed
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    let host = if let Some(rest) = s.strip_prefix('[') {
        // IPv6 bracket form: [::1]:port
        let end = rest
            .find(']')
            .ok_or_else(|| format!("ILINK_HUB_ADDR `{trimmed}` has invalid IPv6 brackets"))?;
        &rest[..end]
    } else {
        s.rsplit_once(':')
            .map(|(h, _)| h)
            .ok_or_else(|| format!("ILINK_HUB_ADDR `{trimmed}` is missing a port"))?
    };

    let host_ok = matches!(host, "127.0.0.1" | "localhost" | "::1");
    if host_ok {
        Ok(())
    } else {
        Err(format!(
            "ILINK_HUB_ADDR `{trimmed}` must use a loopback host \
             (127.0.0.1, localhost, or ::1); got host `{host}`"
        ))
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

pub(crate) fn parse_loopback_port(addr: &str) -> Option<u16> {
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
pub(crate) fn get_desktop_settings<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> DesktopSettingsPayload {
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
pub(crate) fn set_listen_port<R: tauri::Runtime>(app: tauri::AppHandle<R>, port: u16) -> SetListenPortResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScopedHome, PORT_OVERRIDE_LOCK};

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
    fn resolve_initial_listen_addr_rejects_non_loopback_env() {
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        let prev_addr = std::env::var("ILINK_HUB_ADDR").ok();
        std::env::set_var("ILINK_HUB_ADDR", "0.0.0.0:8765");
        let err = resolve_initial_listen_addr().expect_err("non-loopback must fail");
        assert!(
            err.contains("loopback") || err.contains("0.0.0.0"),
            "expected loopback rejection, got: {err}"
        );

        std::env::set_var("ILINK_HUB_ADDR", "127.0.0.1:8765");
        let ok = resolve_initial_listen_addr().expect("loopback must succeed");
        assert_eq!(ok, "127.0.0.1:8765");

        match prev_addr {
            Some(v) => std::env::set_var("ILINK_HUB_ADDR", v),
            None => std::env::remove_var("ILINK_HUB_ADDR"),
        }
    }

    #[test]
    fn setup_fallback_never_reuses_rejected_non_loopback_env() {
        // M2 f1: when resolve fails because ILINK_HUB_ADDR is non-loopback,
        // setup must use the hardcoded safe default — NOT re-read the env.
        let _guard = PORT_OVERRIDE_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = ScopedHome::set(tmp.path());

        let prev_addr = std::env::var("ILINK_HUB_ADDR").ok();
        std::env::set_var("ILINK_HUB_ADDR", "0.0.0.0:8765");

        let err = resolve_initial_listen_addr().expect_err("non-loopback must fail");
        let fallback = safe_listen_addr_on_resolve_error(err);
        assert_eq!(
            fallback, SAFE_LOOPBACK_LISTEN_ADDR,
            "fallback must be hardcoded loopback, never the rejected env value"
        );
        assert!(
            !fallback.starts_with("0.0.0.0"),
            "must not bind all interfaces: {fallback}"
        );
        // Env is still the rejected value — proving fallback did not re-read it.
        assert_eq!(
            std::env::var("ILINK_HUB_ADDR").as_deref(),
            Ok("0.0.0.0:8765")
        );

        match prev_addr {
            Some(v) => std::env::set_var("ILINK_HUB_ADDR", v),
            None => std::env::remove_var("ILINK_HUB_ADDR"),
        }
    }

    #[test]
    fn ensure_loopback_listen_addr_accepts_loopback_hosts() {
        assert!(ensure_loopback_listen_addr("127.0.0.1:8765").is_ok());
        assert!(ensure_loopback_listen_addr("localhost:9000").is_ok());
        assert!(ensure_loopback_listen_addr("[::1]:8765").is_ok());
        assert!(ensure_loopback_listen_addr("::1:8765").is_ok());
        assert!(ensure_loopback_listen_addr("9123").is_ok());
        assert!(ensure_loopback_listen_addr("0.0.0.0:8765").is_err());
        assert!(ensure_loopback_listen_addr("192.168.1.10:8765").is_err());
    }
}
