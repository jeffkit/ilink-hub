//! Shared test helpers for desktop listen-addr / port-override tests.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

/// Serialize port-override tests so they don't step on the global data dir.
/// The desktop-port.json path is a real on-disk artifact under
/// `~/.ilink-hub`, and we don't want parallel tests racing to read/write
/// the same file in CI.
pub static PORT_OVERRIDE_LOCK: StdMutex<()> = StdMutex::new(());

/// `HOME`-shaped environment for `resolve_initial_listen_addr` to inspect.
pub struct ScopedHome {
    previous: Option<String>,
    original: PathBuf,
}

impl ScopedHome {
    pub fn set(home: &Path) -> Self {
        let previous = std::env::var("HOME").ok();
        let original = ilink_hub::paths::data_dir();
        // `data_dir()` reads `dirs::home_dir()`, which on Unix consults
        // `HOME` first; set it for the duration of the test.
        std::env::set_var("HOME", home);
        Self { previous, original }
    }
}

impl Drop for ScopedHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        // Touch the original to make sure the value was actually read by
        // dirs; this is a no-op but documents intent.
        let _ = self.original;
    }
}
