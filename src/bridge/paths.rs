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
    use std::fs;
    use std::io::Write;

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

    /// Catches `resolve_bridge_executable -> Default::default()` (empty PathBuf).
    #[test]
    fn resolve_bridge_executable_never_returns_empty_path() {
        let resolved = resolve_bridge_executable();
        assert!(
            !resolved.as_os_str().is_empty(),
            "must return a non-empty path, got {resolved:?}"
        );
    }

    /// Catches `delete !` on `!trimmed.is_empty()`: blank override must fall through.
    #[test]
    fn resolve_bridge_executable_ignores_blank_override_env() {
        temp_env::with_vars(
            [
                ("ILINKHUB_BRIDGE_EXE", Some("   ")),
                // Clear PATH so we don't accidentally pick a real binary.
                ("PATH", Some("")),
            ],
            || {
                let resolved = resolve_bridge_executable();
                // Blank override must not win; either sibling/current or bare name.
                assert_ne!(
                    resolved,
                    PathBuf::from("   "),
                    "whitespace-only override must be ignored"
                );
                assert!(
                    !resolved.as_os_str().is_empty(),
                    "fallback must still yield a non-empty path"
                );
            },
        );
    }

    /// Catches override path being preferred when non-empty.
    #[test]
    fn resolve_bridge_executable_uses_non_empty_override_env() {
        temp_env::with_var(
            "ILINKHUB_BRIDGE_EXE",
            Some("/tmp/custom-ilink-hub-bridge"),
            || {
                assert_eq!(
                    resolve_bridge_executable(),
                    PathBuf::from("/tmp/custom-ilink-hub-bridge")
                );
            },
        );
    }

    #[test]
    fn is_bridge_executable_matches_exact_binary_name() {
        assert!(is_bridge_executable(Path::new("/opt/bin/ilink-hub-bridge")));
        assert!(is_bridge_executable(Path::new("ilink-hub-bridge")));
    }

    #[test]
    fn is_bridge_executable_rejects_other_names() {
        assert!(!is_bridge_executable(Path::new("/opt/bin/ilink-hub")));
        assert!(!is_bridge_executable(Path::new(
            "/opt/bin/ilink-hub-bridge-extra"
        )));
        assert!(!is_bridge_executable(Path::new("/opt/bin/")));
        assert!(!is_bridge_executable(Path::new("")));
    }

    #[test]
    fn sibling_bridge_executable_returns_existing_sibling_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current = dir.path().join("ilink-hub");
        fs::File::create(&current).expect("touch current");
        let sibling = dir.path().join(BRIDGE_BINARY_FILE);
        fs::File::create(&sibling).expect("touch sibling");

        let found = sibling_bridge_executable(&current);
        assert_eq!(found.as_deref(), Some(sibling.as_path()));
    }

    #[test]
    fn sibling_bridge_executable_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current = dir.path().join("ilink-hub");
        fs::File::create(&current).expect("touch current");
        assert!(sibling_bridge_executable(&current).is_none());
    }

    #[test]
    fn find_in_path_returns_none_when_path_unset() {
        temp_env::with_var_unset("PATH", || {
            assert!(find_in_path(BRIDGE_BINARY_FILE).is_none());
        });
    }

    #[test]
    fn find_in_path_locates_file_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("fake-bridge-bin");
        {
            let mut f = fs::File::create(&bin).expect("create bin");
            writeln!(f, "#!/bin/sh").ok();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }

        temp_env::with_var("PATH", Some(dir.path()), || {
            let found = find_in_path("fake-bridge-bin");
            assert_eq!(found.as_deref(), Some(bin.as_path()));
        });
    }

    #[test]
    fn find_in_path_returns_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        temp_env::with_var("PATH", Some(dir.path()), || {
            assert!(find_in_path("definitely-not-on-path-xyz").is_none());
        });
    }
}
