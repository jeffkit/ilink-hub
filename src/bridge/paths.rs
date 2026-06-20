use std::path::{Path, PathBuf};

#[cfg(windows)]
const BRIDGE_BINARY_FILE: &str = "ilink-hub-bridge.exe";
#[cfg(not(windows))]
const BRIDGE_BINARY_FILE: &str = "ilink-hub-bridge";

/// Resolve the `ilink-hub-bridge` executable for spawning child bridge processes.
///
/// When the current process is already `ilink-hub-bridge`, returns `current_exe()`.
/// Otherwise checks `ILINKHUB_BRIDGE_EXE`, a sibling binary next to `current_exe`,
/// then `PATH`. Falls back to the bare command name.
pub fn resolve_bridge_executable() -> PathBuf {
    if let Ok(override_path) = std::env::var("ILINKHUB_BRIDGE_EXE") {
        let trimmed = override_path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Ok(current) = std::env::current_exe() {
        if is_bridge_executable(&current) {
            return current;
        }
        if let Some(sibling) = sibling_bridge_executable(&current) {
            return sibling;
        }
    }

    if let Some(from_path) = find_in_path(BRIDGE_BINARY_FILE) {
        return from_path;
    }

    PathBuf::from(BRIDGE_BINARY_FILE)
}

pub(super) fn is_bridge_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|name| {
            #[cfg(windows)]
            {
                name.eq_ignore_ascii_case("ilink-hub-bridge.exe")
            }
            #[cfg(not(windows))]
            {
                name == "ilink-hub-bridge"
            }
        })
        .unwrap_or(false)
}

pub(super) fn sibling_bridge_executable(current: &Path) -> Option<PathBuf> {
    let dir = current.parent()?;
    let sibling = dir.join(BRIDGE_BINARY_FILE);
    if sibling.is_file() {
        Some(sibling)
    } else {
        None
    }
}

pub(crate) fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

/// Find a tool executable, falling back to common user-local install paths when the
/// tool is not found in the current `PATH`.
///
/// When the bridge manager is started via `launchctl` / LaunchAgent the inherited
/// `PATH` is the minimal system PATH and does not include user-local dirs such as
/// `~/.local/bin` where tools like the Cursor `agent` CLI are installed.
///
/// Search order:
/// 1. Current `PATH` (fast path — works in normal terminal sessions)
/// 2. `$HOME/.local/bin` (most common user-local install on macOS/Linux)
/// 3. `$HOME/bin`
/// 4. Falls back to the bare name so the OS error message is descriptive.
pub(crate) fn find_tool_with_extra_paths(name: &str) -> PathBuf {
    if let Some(p) = find_in_path(name) {
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        for subdir in &[".local/bin", "bin"] {
            let candidate = PathBuf::from(&home).join(subdir).join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// Resolve the executable for built-in self-invocation (`ilink-hub-bridge profile …`).
pub(super) fn resolve_spawn_command(command: &str) -> String {
    if command == "ilink-hub-bridge" {
        return resolve_bridge_executable().to_string_lossy().into_owned();
    }
    command.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_spawn_command_uses_bridge_executable_for_self_invoke() {
        let resolved = resolve_spawn_command("ilink-hub-bridge");
        assert_eq!(resolved, resolve_bridge_executable().to_string_lossy());
    }

    #[test]
    fn resolve_spawn_command_passthrough_other_commands() {
        assert_eq!(resolve_spawn_command("claude"), "claude");
    }

    #[test]
    fn resolve_bridge_executable_prefers_current_exe_when_already_bridge() {
        if let Ok(exe) = std::env::current_exe() {
            if is_bridge_executable(&exe) {
                assert_eq!(resolve_bridge_executable(), exe);
            }
        }
    }

    #[test]
    fn resolve_bridge_executable_falls_back_to_command_name() {
        if std::env::var_os("ILINKHUB_BRIDGE_EXE").is_some() {
            return;
        }
        if let Ok(exe) = std::env::current_exe() {
            if is_bridge_executable(&exe) || sibling_bridge_executable(&exe).is_some() {
                return;
            }
            if find_in_path(BRIDGE_BINARY_FILE).is_some() {
                return;
            }
        }
        assert_eq!(
            resolve_bridge_executable(),
            PathBuf::from(BRIDGE_BINARY_FILE)
        );
    }
}
