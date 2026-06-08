//! Client registry — tracks registered backend clients and their virtual tokens.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Stable name chosen by the client (e.g. "mac-workspace", "server")
    pub name: String,
    /// Virtual token issued by the Hub; client uses this as its `bot_token`
    pub vtoken: String,
    /// Human-readable label shown in `/list`
    pub label: Option<String>,
    pub registered_at: Instant,
    pub last_seen: Option<Instant>,
    pub online: bool,
}

impl ClientInfo {
    pub fn new(name: String, label: Option<String>) -> Self {
        Self {
            name,
            vtoken: format!("vhub_{}", Uuid::new_v4().simple()),
            label,
            registered_at: Instant::now(),
            last_seen: None,
            online: false,
        }
    }
}

#[derive(Debug)]
pub struct ClientRegistry {
    /// vtoken → ClientInfo
    by_vtoken: HashMap<String, ClientInfo>,
    /// name → vtoken
    by_name: HashMap<String, String>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            by_vtoken: HashMap::new(),
            by_name: HashMap::new(),
        }
    }

    /// Register a new client, returning its virtual token.
    /// If a client with the same name already exists, its vtoken is reused.
    pub fn register(&mut self, name: String, label: Option<String>) -> String {
        self.register_with_vtoken(name, label, None)
    }

    /// Register a client with a specific vtoken (used when loading from DB on startup).
    /// If name already exists, the existing entry is updated; vtoken argument is ignored.
    /// If name is new and vtoken is provided, that vtoken is used; otherwise a fresh one is generated.
    pub fn register_with_vtoken(
        &mut self,
        name: String,
        label: Option<String>,
        vtoken: Option<String>,
    ) -> String {
        if let Some(existing_vtoken) = self.by_name.get(&name) {
            let existing_vtoken = existing_vtoken.clone();
            if let Some(info) = self.by_vtoken.get_mut(&existing_vtoken) {
                if label.is_some() {
                    info.label = label;
                }
                info.online = true;
                info.last_seen = Some(Instant::now());
            }
            return existing_vtoken;
        }

        let mut info = ClientInfo::new(name.clone(), label);
        if let Some(vt) = vtoken {
            info.vtoken = vt;
        }
        let vtoken = info.vtoken.clone();
        self.by_name.insert(name, vtoken.clone());
        self.by_vtoken.insert(vtoken.clone(), info);
        vtoken
    }

    pub fn get_by_vtoken(&self, vtoken: &str) -> Option<&ClientInfo> {
        self.by_vtoken.get(vtoken)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&ClientInfo> {
        self.by_name.get(name).and_then(|vt| self.by_vtoken.get(vt))
    }

    pub fn mark_seen(&mut self, vtoken: &str) {
        if let Some(info) = self.by_vtoken.get_mut(vtoken) {
            info.last_seen = Some(Instant::now());
            info.online = true;
        }
    }

    pub fn evict_stale(&mut self, timeout: Duration) {
        let now = Instant::now();
        for info in self.by_vtoken.values_mut() {
            if let Some(last) = info.last_seen {
                if now.duration_since(last) > timeout {
                    info.online = false;
                }
            }
        }
    }

    pub fn online_clients(&self) -> Vec<&ClientInfo> {
        self.by_vtoken.values().filter(|c| c.online).collect()
    }

    pub fn all_clients(&self) -> Vec<&ClientInfo> {
        self.by_vtoken.values().collect()
    }

    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(vtoken) = self.by_name.remove(name) {
            self.by_vtoken.remove(&vtoken);
            true
        } else {
            false
        }
    }

    /// Remove routing entries that pointed at `vtoken`. Clears default if it matched.
    /// Returns the new default vtoken, if any.
    pub fn pick_default_after_remove(&self, removed_vtoken: &str) -> Option<String> {
        self.online_clients()
            .iter()
            .find(|c| c.vtoken != removed_vtoken)
            .map(|c| c.vtoken.clone())
            .or_else(|| {
                self.all_clients()
                    .iter()
                    .find(|c| c.vtoken != removed_vtoken)
                    .map(|c| c.vtoken.clone())
            })
    }
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_count_single_vs_multi() {
        let mut reg = ClientRegistry::new();
        assert_eq!(reg.all_clients().len(), 0);
        reg.register("a".into(), None);
        assert_eq!(reg.all_clients().len(), 1);
        reg.register("b".into(), Some("B".into()));
        assert_eq!(reg.all_clients().len(), 2);
    }

    #[test]
    fn register_same_name_reuses_vtoken() {
        let mut reg = ClientRegistry::new();
        let v1 = reg.register("w".into(), None);
        let v2 = reg.register("w".into(), Some("lbl".into()));
        assert_eq!(v1, v2);
        assert_eq!(reg.all_clients().len(), 1);
    }

    #[test]
    fn get_by_name_roundtrip() {
        let mut reg = ClientRegistry::new();
        reg.register("echo".into(), Some("echo test".into()));
        let c = reg.get_by_name("echo").expect("client");
        assert_eq!(c.name, "echo");
        assert_eq!(c.label.as_deref(), Some("echo test"));
        assert!(reg.get_by_vtoken(&c.vtoken).is_some());
    }
}
