//! Client registry — tracks registered backend clients and their virtual tokens.
//!
//! Post-M1: `ClientInfo::vtoken` and the `by_vtoken` map key are the
//! SHA-256 hex of the plaintext vtoken issued at registration. The plaintext
//! is returned to the bridge exactly once (via [`ClientRegistry::register`])
//! and is not retained anywhere — DB, registry, queue keys, and last-seen
//! DashMap all use the hash form. Callers that have the plaintext (HTTP
//! bearer credentials from the bridge) must hash it before invoking
//! `get_by_vtoken` / `mark_online` / `mark_offline`.

use std::collections::HashMap;
use std::time::SystemTime;
use uuid::Uuid;

use super::hash_vtoken;

#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Stable name chosen by the client (e.g. "mac-workspace", "server")
    pub name: String,
    /// SHA-256 hex of the plaintext virtual token issued at registration.
    /// The plaintext is returned to the caller by `register` exactly once
    /// and is not stored in this struct.
    pub vtoken: String,
    /// Human-readable label shown in `/list`
    pub label: Option<String>,
    /// Detailed description of the Agent's capabilities.
    /// Exposed via the MCP `list_agents` tool so other Agents can understand
    /// what this Agent can do before calling it.
    pub description: Option<String>,
    /// Wall-clock registration time; survives display across restarts.
    pub registered_at: SystemTime,
    pub online: bool,
    /// Optional persona display name prepended to every outbound reply (e.g. "Claude").
    pub persona_name: Option<String>,
    /// Optional emoji avatar accompanying `persona_name` (e.g. "🤖").
    pub persona_emoji: Option<String>,
}

impl ClientInfo {
    /// Build a `ClientInfo` for a freshly-issued plaintext vtoken. Returns a tuple
    /// of the constructed `ClientInfo` (holding the hashed token) and the `String`
    /// plaintext token.
    pub fn new(name: String, label: Option<String>, description: Option<String>) -> (Self, String) {
        let plain = format!("vhub_{}", Uuid::new_v4().simple());
        let hashed = hash_vtoken(&plain);
        (
            Self::with_hashed_vtoken(name, label, description, hashed),
            plain,
        )
    }

    /// Build a `ClientInfo` whose `vtoken` field is already the canonical
    /// hash. Used by the startup loader (`load_clients_from_db`) which
    /// reads hash values directly from the `clients` table and must NOT
    /// re-hash them.
    fn with_hashed_vtoken(
        name: String,
        label: Option<String>,
        description: Option<String>,
        vtoken_hash: String,
    ) -> Self {
        Self {
            name,
            vtoken: vtoken_hash,
            label,
            description,
            registered_at: SystemTime::now(),
            online: false,
            persona_name: None,
            persona_emoji: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateClientError {
    NotFound,
    NameTaken,
}

#[derive(Debug)]
pub struct ClientRegistry {
    /// hashed vtoken → ClientInfo
    by_vtoken: HashMap<String, ClientInfo>,
    /// name → hashed vtoken
    by_name: HashMap<String, String>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            by_vtoken: HashMap::new(),
            by_name: HashMap::new(),
        }
    }

    /// Register a new client.
    /// If a client with the same name already exists, its existing entry
    /// is updated (label / online) and the existing hash is returned; the
    /// plaintext is the empty string since the original plaintext is
    /// unknown to the registry.
    /// If the name is new, a fresh plaintext is generated, hashed, and
    /// stored. The returned tuple is `(plaintext, hashed, is_new)`:
    /// - `plaintext` is the bearer credential the bridge should use. It
    ///   is only set on the fresh-registration path; on the existing-name
    ///   path it is `""` because the original plaintext was returned in a
    ///   previous call and the registry never retains it.
    /// - `hashed` is the SHA-256 hex stored in `by_vtoken` / `ClientInfo`.
    ///   Callers that need to write to the store should use this value
    ///   (the store binds the hash, never the plaintext).
    /// - `is_new` is `true` only when this call inserted a fresh row. If
    ///   the name was already registered, the existing entry is preserved
    ///   and `is_new = false`. Callers that need to roll back a
    ///   speculative register MUST honour `is_new`: rolling back a reused
    ///   entry evicts the legitimate client (F-M1-A).
    pub fn register(
        &mut self,
        name: String,
        label: Option<String>,
        description: Option<String>,
    ) -> (String, String, bool) {
        if let Some(existing_hash) = self.by_name.get(&name).cloned() {
            if let Some(info) = self.by_vtoken.get_mut(&existing_hash) {
                if label.is_some() {
                    info.label = label;
                }
                if description.is_some() {
                    info.description = description;
                }
                info.online = true;
            }
            return (String::new(), existing_hash, false);
        }
        let (info, plain) = ClientInfo::new(name.clone(), label, description);
        let hashed = info.vtoken.clone();
        self.by_name.insert(name, hashed.clone());
        self.by_vtoken.insert(hashed.clone(), info);
        (plain, hashed, true)
    }

    /// Register a client with a specific vtoken (used when loading from DB on startup).
    /// If name already exists, the existing entry is updated; the supplied
    /// vtoken is ignored in that case.
    /// If name is new and vtoken is provided, that vtoken is treated as the
    /// canonical hash (the DB has already hashed plaintexts at write time)
    /// and stored verbatim — no second hash pass.
    /// If name is new and vtoken is None, a fresh plaintext is generated,
    /// hashed, and stored (the returned value is the plaintext, identical
    /// to `register`).
    /// Returns `(stored_vtoken, is_new)`. The string is the plaintext when
    /// freshly generated, and the hash when supplied (load path) or when
    /// the name was already registered.
    pub fn register_with_vtoken(
        &mut self,
        name: String,
        label: Option<String>,
        description: Option<String>,
        vtoken: Option<String>,
    ) -> (String, bool) {
        if let Some(existing_vtoken) = self.by_name.get(&name) {
            let existing_vtoken = existing_vtoken.clone();
            if let Some(info) = self.by_vtoken.get_mut(&existing_vtoken) {
                if label.is_some() {
                    info.label = label;
                }
                if description.is_some() {
                    info.description = description;
                }
                info.online = true;
            }
            return (existing_vtoken, false);
        }

        let (info, plain_or_hash) = match vtoken {
            Some(hashed) => {
                // Caller is the load path; the value is already the canonical hash.
                let info = ClientInfo::with_hashed_vtoken(
                    name.clone(),
                    label,
                    description,
                    hashed.clone(),
                );
                (info, hashed)
            }
            None => {
                // Fresh registration: generate plaintext, hash, store, and return plaintext.
                let (info, plain) = ClientInfo::new(name.clone(), label, description);
                (info, plain)
            }
        };
        let stored = info.vtoken.clone();
        self.by_name.insert(name, stored.clone());
        self.by_vtoken.insert(stored, info);
        (plain_or_hash, true)
    }

    /// Register a client that has been confirmed via pairing. This inserts the
    /// client directly into the registry using the pre-computed hash.
    /// Returns `Err(UpdateClientError::NameTaken)` if the name is already registered.
    pub fn register_confirmed(
        &mut self,
        name: String,
        label: Option<String>,
        description: Option<String>,
        vtoken_hash: String,
    ) -> Result<(), UpdateClientError> {
        if self.by_name.contains_key(&name) {
            return Err(UpdateClientError::NameTaken);
        }
        let info =
            ClientInfo::with_hashed_vtoken(name.clone(), label, description, vtoken_hash.clone());
        self.by_name.insert(name, vtoken_hash.clone());
        self.by_vtoken.insert(vtoken_hash, info);
        Ok(())
    }

    /// Look up a client by the **hashed** form of its vtoken. HTTP handlers
    /// that receive the plaintext bearer credential must hash it before
    /// calling this.
    pub fn get_by_vtoken(&self, vtoken: &str) -> Option<&ClientInfo> {
        self.by_vtoken.get(vtoken)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&ClientInfo> {
        self.by_name.get(name).and_then(|vt| self.by_vtoken.get(vt))
    }

    /// Resolve a client by its registered name **or** a numeric alias.
    ///
    /// `@1`, `@2`, … and `/use 1`, `/use 2`, … map to the n-th client in the
    /// `/list` output. The ordering is `name` ascending (dictionary order) so
    /// the index is stable and predictable across calls — `all_clients()`
    /// returns a HashMap iteration order which must NOT be used directly.
    ///
    /// Resolution order: exact name first (so a client genuinely named `"1"`
    /// still wins), then the 1-based numeric index into the sorted list.
    pub fn get_by_alias(&self, name: &str) -> Option<&ClientInfo> {
        self.get_by_alias_in(name, None)
    }

    /// [`Self::get_by_alias`] restricted to a caller-visible allowlist.
    ///
    /// `allowed = None` means "no restriction"; `Some(set)` means only clients
    /// whose name is in `set` are visible. Both the exact-name and the numeric
    /// alias path honour the filter, and the numeric index is computed over the
    /// **visible** set only — otherwise `/use 2` could still reach a hidden
    /// client that the filtered `/list` never showed.
    pub fn get_by_alias_in(
        &self,
        name: &str,
        allowed: Option<&std::collections::HashSet<String>>,
    ) -> Option<&ClientInfo> {
        if let Some(c) = self.get_by_name(name) {
            return allowed.is_none_or(|set| set.contains(&c.name)).then_some(c);
        }
        let n: usize = name.parse().ok()?;
        if n == 0 {
            return None;
        }
        let mut sorted: Vec<&ClientInfo> = self.all_clients_in(allowed);
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        sorted.get(n - 1).copied()
    }

    /// Update metadata fields on an already-registered client (identified by hashed vtoken).
    /// Used by the startup loader and the admin update API.
    pub fn update_metadata(
        &mut self,
        vtoken: &str,
        label: Option<String>,
        description: Option<String>,
        persona_name: Option<String>,
        persona_emoji: Option<String>,
    ) {
        if let Some(info) = self.by_vtoken.get_mut(vtoken) {
            if label.is_some() {
                info.label = label;
            }
            if description.is_some() {
                info.description = description;
            }
            if persona_name.is_some() {
                info.persona_name = persona_name;
            }
            if persona_emoji.is_some() {
                info.persona_emoji = persona_emoji;
            }
        }
    }

    /// Set persona fields on an already-registered client (identified by hashed vtoken).
    /// Used by the startup loader and the admin update API.
    /// Deprecated: use `update_metadata` instead.
    pub fn set_persona(
        &mut self,
        vtoken: &str,
        persona_name: Option<String>,
        persona_emoji: Option<String>,
    ) {
        self.update_metadata(vtoken, None, None, persona_name, persona_emoji);
    }

    /// Set the description for a registered client.
    pub fn set_description(&mut self, vtoken: &str, description: Option<String>) {
        if let Some(info) = self.by_vtoken.get_mut(vtoken) {
            info.description = description;
        }
    }

    /// Mark a client as online. The `vtoken` argument must be the hashed
    /// form (callers receive the plaintext bearer credential over HTTP and
    /// must hash before calling).
    pub fn mark_online(&mut self, vtoken: &str) {
        if let Some(info) = self.by_vtoken.get_mut(vtoken) {
            info.online = true;
        }
    }

    /// Mark a client as offline. Called by the health checker after it detects
    /// the last-seen timestamp has exceeded the stale threshold. The
    /// `vtoken` argument is the hashed form (the DashMap key under
    /// `state.clients.last_seen` is the hash, so the same value reaches us).
    pub fn mark_offline(&mut self, vtoken: &str) {
        if let Some(info) = self.by_vtoken.get_mut(vtoken) {
            info.online = false;
        }
    }

    pub fn online_clients(&self) -> Vec<&ClientInfo> {
        self.by_vtoken.values().filter(|c| c.online).collect()
    }

    pub fn all_clients(&self) -> Vec<&ClientInfo> {
        self.all_clients_in(None)
    }

    /// All clients, optionally restricted to a caller-visible allowlist
    /// (`None` = no restriction, `Some(set)` = only names in `set`).
    pub fn all_clients_in(
        &self,
        allowed: Option<&std::collections::HashSet<String>>,
    ) -> Vec<&ClientInfo> {
        match allowed {
            None => self.by_vtoken.values().collect(),
            Some(set) => self
                .by_vtoken
                .values()
                .filter(|c| set.contains(&c.name))
                .collect(),
        }
    }

    /// Mint a fresh vtoken for an existing client and swap it in place.
    ///
    /// Used by the admin revoke endpoint: the old hash loses all authority
    /// (its `by_vtoken` entry and every `by_name` pointer to it are replaced),
    /// while label / description / persona / registration time survive so the
    /// client keeps its identity. The client is marked offline — the bridge
    /// still holds the old bearer credential and must re-pair with the
    /// returned plaintext.
    ///
    /// Returns `(plaintext, hashed)` for the rotated token, or `None` when no
    /// client is registered under `name`.
    pub fn rotate_vtoken(&mut self, name: &str) -> Option<(String, String)> {
        let old_hash = self.by_name.get(name)?.clone();
        let mut info = self.by_vtoken.remove(&old_hash)?;

        let plain = format!("vhub_{}", Uuid::new_v4().simple());
        let new_hash = hash_vtoken(&plain);

        info.vtoken = new_hash.clone();
        info.online = false;
        self.by_vtoken.insert(new_hash.clone(), info);
        self.by_name.insert(name.to_string(), new_hash.clone());

        Some((plain, new_hash))
    }

    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(vtoken) = self.by_name.remove(name) {
            self.by_vtoken.remove(&vtoken);
            true
        } else {
            false
        }
    }

    /// Update a client's name and/or label. Returns the client's hashed
    /// vtoken on success.
    pub fn update_client(
        &mut self,
        old_name: &str,
        new_name: &str,
        label: Option<String>,
    ) -> Result<String, UpdateClientError> {
        let vtoken = self
            .by_name
            .get(old_name)
            .cloned()
            .ok_or(UpdateClientError::NotFound)?;

        if new_name != old_name && self.by_name.contains_key(new_name) {
            return Err(UpdateClientError::NameTaken);
        }

        let info = self
            .by_vtoken
            .get_mut(&vtoken)
            .ok_or(UpdateClientError::NotFound)?;

        if new_name != old_name {
            self.by_name.remove(old_name);
            info.name = new_name.to_string();
            self.by_name.insert(new_name.to_string(), vtoken.clone());
        }

        if let Some(l) = label {
            info.label = Some(l);
        }
        Ok(vtoken)
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
    use crate::hub::is_vtoken_hash;

    #[test]
    fn registered_count_single_vs_multi() {
        let mut reg = ClientRegistry::new();
        assert_eq!(reg.all_clients().len(), 0);
        let (plain1, _, is_new1) = reg.register("a".into(), None, None);
        assert!(is_new1);
        assert!(!plain1.is_empty());
        assert!(
            plain1.starts_with("vhub_"),
            "plaintext vtoken must be returned exactly once"
        );
        assert_eq!(reg.all_clients().len(), 1);
        let (plain2, _, is_new2) = reg.register("b".into(), Some("B".into()), None);
        assert!(is_new2);
        assert!(!plain2.is_empty());
        assert!(plain2.starts_with("vhub_"));
        assert_eq!(reg.all_clients().len(), 2);
    }

    #[test]
    fn register_same_name_reuses_vtoken() {
        let mut reg = ClientRegistry::new();
        let (_plain1, hash1, is_new1) = reg.register("w".into(), None, None);
        assert!(is_new1);
        // Re-registration of the same name does not mint a new plaintext;
        // the registry returns the existing hash and an empty plaintext.
        let (plain2, hash2, is_new2) = reg.register("w".into(), Some("lbl".into()), None);
        assert!(!is_new2, "second register of same name is NOT new");
        assert!(plain2.is_empty(), "no new plaintext on re-registration");
        assert_eq!(hash1, hash2, "same name reuses hashed vtoken");
        assert_eq!(reg.all_clients().len(), 1);
    }

    #[test]
    fn stored_vtoken_is_hash_not_plaintext() {
        let mut reg = ClientRegistry::new();
        let (plain, hashed, _) = reg.register("echo".into(), Some("echo test".into()), None);
        let c = reg.get_by_name("echo").expect("client");
        assert_ne!(
            c.vtoken, plain,
            "ClientInfo.vtoken must NOT hold the plaintext"
        );
        assert_eq!(
            c.vtoken, hashed,
            "stored vtoken must equal the hashed return value"
        );
        assert!(
            is_vtoken_hash(&c.vtoken),
            "stored vtoken must be a SHA-256 hex; got {:?}",
            c.vtoken
        );
        // Round-trip: hash(plain) == stored hash
        assert_eq!(c.vtoken, crate::hub::hash_vtoken(&plain));
    }

    #[test]
    fn get_by_vtoken_uses_hash() {
        let mut reg = ClientRegistry::new();
        let (plain, _, _) = reg.register("a".into(), None, None);
        let hashed = crate::hub::hash_vtoken(&plain);
        assert!(reg.get_by_vtoken(&hashed).is_some());
        assert!(
            reg.get_by_vtoken(&plain).is_none(),
            "get_by_vtoken must NOT match plaintext"
        );
    }

    #[test]
    fn get_by_name_roundtrip() {
        let mut reg = ClientRegistry::new();
        reg.register("echo".into(), Some("echo test".into()), None);
        let c = reg.get_by_name("echo").expect("client");
        assert_eq!(c.name, "echo");
        assert_eq!(c.label.as_deref(), Some("echo test"));
        // already-hashed lookup should also work
        assert!(reg.get_by_vtoken(&c.vtoken).is_some());
    }

    #[test]
    fn get_by_alias_matches_exact_name_first() {
        let mut reg = ClientRegistry::new();
        // Register in non-sorted insertion order to prove the numeric path
        // does not depend on insertion order.
        reg.register("charlie".into(), None, None);
        reg.register("alpha".into(), None, None);
        reg.register("bravo".into(), None, None);
        // Exact name wins even if a numeric alias could also resolve.
        assert_eq!(reg.get_by_alias("alpha").unwrap().name, "alpha");
        assert_eq!(reg.get_by_alias("bravo").unwrap().name, "bravo");
    }

    #[test]
    fn get_by_alias_numeric_uses_sorted_index() {
        let mut reg = ClientRegistry::new();
        reg.register("charlie".into(), None, None);
        reg.register("alpha".into(), None, None);
        reg.register("bravo".into(), None, None);
        // Sorted: alpha(1), bravo(2), charlie(3)
        assert_eq!(reg.get_by_alias("1").unwrap().name, "alpha");
        assert_eq!(reg.get_by_alias("2").unwrap().name, "bravo");
        assert_eq!(reg.get_by_alias("3").unwrap().name, "charlie");
    }

    #[test]
    fn get_by_alias_rejects_zero_and_overflow() {
        let mut reg = ClientRegistry::new();
        reg.register("alpha".into(), None, None);
        assert!(reg.get_by_alias("0").is_none(), "0 is not a valid alias");
        assert!(
            reg.get_by_alias("2").is_none(),
            "out-of-range index must return None"
        );
    }

    #[test]
    fn get_by_alias_unknown_name_returns_none() {
        let mut reg = ClientRegistry::new();
        reg.register("alpha".into(), None, None);
        assert!(reg.get_by_alias("nope").is_none());
        assert!(
            reg.get_by_alias("999").is_none(),
            "non-existent numeric alias must return None"
        );
    }

    #[test]
    fn get_by_alias_numeric_works_on_empty_registry() {
        let reg = ClientRegistry::new();
        assert!(reg.get_by_alias("1").is_none());
    }

    #[test]
    fn mark_online_offline_uses_hash() {
        let mut reg = ClientRegistry::new();
        let (_plain, hashed, _) = reg.register("a".into(), None, None);
        reg.mark_online(&hashed);
        assert!(reg.get_by_name("a").unwrap().online);
        reg.mark_offline(&hashed);
        assert!(!reg.get_by_name("a").unwrap().online);
        // plaintext must NOT flip the state
        let (plain2, _, _) = reg.register("b".into(), None, None);
        reg.mark_online(&plain2);
        assert!(
            !reg.get_by_name("b").unwrap().online,
            "mark_online with plaintext must be a no-op"
        );
    }

    #[test]
    fn update_client_renames_and_updates_label() {
        let mut reg = ClientRegistry::new();
        let (_, hashed, _) = reg.register("old".into(), Some("old label".into()), None);
        reg.update_client("old", "new", Some("new label".into()))
            .unwrap();
        assert!(reg.get_by_name("old").is_none());
        let c = reg.get_by_name("new").expect("renamed");
        assert_eq!(c.vtoken, hashed);
        assert_eq!(c.label.as_deref(), Some("new label"));
    }

    #[test]
    fn update_client_rejects_duplicate_name() {
        let mut reg = ClientRegistry::new();
        reg.register("a".into(), None, None);
        reg.register("b".into(), None, None);
        assert_eq!(
            reg.update_client("a", "b", None),
            Err(UpdateClientError::NameTaken)
        );
    }

    #[test]
    fn register_with_vtoken_load_path_does_not_rehash() {
        // Simulate the load_clients_from_db path: caller has the DB-side hash
        // and wants to insert it without hashing a second time.
        let mut reg = ClientRegistry::new();
        let (_, _, _) = reg.register("first".into(), None, None);
        let stored_first_hash = reg.get_by_name("first").unwrap().vtoken.clone();

        // A different name + the SAME hash: the load path takes the supplied
        // value as the canonical hash and does not double-hash.
        let (stored, is_new) =
            reg.register_with_vtoken("second".into(), None, None, Some(stored_first_hash.clone()));
        assert!(is_new);
        assert_eq!(
            stored, stored_first_hash,
            "register_with_vtoken must return the supplied value verbatim"
        );
        let c = reg.get_by_name("second").unwrap();
        assert_eq!(
            c.vtoken, stored_first_hash,
            "register_with_vtoken must store the supplied value verbatim"
        );
    }

    #[test]
    fn register_with_description() {
        let mut reg = ClientRegistry::new();
        let (plain, _hashed, _) = reg.register(
            "agent".into(),
            Some("Test Agent".into()),
            Some("An agent for testing".into()),
        );
        assert!(!plain.is_empty());
        let c = reg.get_by_name("agent").expect("client");
        assert_eq!(c.label.as_deref(), Some("Test Agent"));
        assert_eq!(c.description.as_deref(), Some("An agent for testing"));
    }

    #[test]
    fn register_updates_description() {
        let mut reg = ClientRegistry::new();
        let (_, hash1, _) = reg.register("agent".into(), None, None);
        let (_, hash2, _) = reg.register("agent".into(), None, Some("New description".into()));
        assert_eq!(hash1, hash2, "same name reuses hashed vtoken");
        let c = reg.get_by_name("agent").expect("client");
        assert_eq!(c.description.as_deref(), Some("New description"));
    }

    // ── update_metadata / set_persona / set_description ───────────────

    #[test]
    fn update_metadata_persists_all_fields() {
        let mut reg = ClientRegistry::new();
        let (_, hash, _) = reg.register("bot".into(), None, None);

        reg.update_metadata(
            &hash,
            Some("my-label".into()),
            Some("desc".into()),
            Some("Aria".into()),
            Some("🤖".into()),
        );

        let c = reg.get_by_vtoken(&hash).expect("client");
        assert_eq!(
            c.label.as_deref(),
            Some("my-label"),
            "label must be updated"
        );
        assert_eq!(
            c.description.as_deref(),
            Some("desc"),
            "description must be updated"
        );
        assert_eq!(
            c.persona_name.as_deref(),
            Some("Aria"),
            "persona_name must be updated"
        );
        assert_eq!(
            c.persona_emoji.as_deref(),
            Some("🤖"),
            "persona_emoji must be updated"
        );
    }

    #[test]
    fn set_persona_persists_persona_fields() {
        let mut reg = ClientRegistry::new();
        let (_, hash, _) = reg.register("bot".into(), None, None);

        reg.set_persona(&hash, Some("Claude".into()), Some("🧠".into()));

        let c = reg.get_by_vtoken(&hash).expect("client");
        assert_eq!(c.persona_name.as_deref(), Some("Claude"));
        assert_eq!(c.persona_emoji.as_deref(), Some("🧠"));
    }

    #[test]
    fn set_description_persists_description() {
        let mut reg = ClientRegistry::new();
        let (_, hash, _) = reg.register("bot".into(), None, None);

        reg.set_description(&hash, Some("helpful assistant".into()));
        assert_eq!(
            reg.get_by_vtoken(&hash).unwrap().description.as_deref(),
            Some("helpful assistant")
        );

        // Clearing description with None
        reg.set_description(&hash, None);
        assert_eq!(
            reg.get_by_vtoken(&hash).unwrap().description,
            None,
            "description must be cleared"
        );
    }

    // ── online_clients ─────────────────────────────────────────────────

    #[test]
    fn online_clients_returns_only_online() {
        let mut reg = ClientRegistry::new();
        let (_, ha, _) = reg.register("a".into(), None, None);
        let (_, hb, _) = reg.register("b".into(), None, None);
        let (_, _hc, _) = reg.register("c".into(), None, None);

        reg.mark_online(&ha);
        reg.mark_online(&hb);
        // c stays offline

        let online: Vec<_> = reg
            .online_clients()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(online.len(), 2, "only 2 online clients");
        assert!(online.contains(&"a"));
        assert!(online.contains(&"b"));
        assert!(!online.contains(&"c"), "offline client must not appear");
    }

    // ── pick_default_after_remove ───────────────────────────────────────

    #[test]
    fn pick_default_after_remove_returns_another_online_client() {
        let mut reg = ClientRegistry::new();
        let (_, ha, _) = reg.register("a".into(), None, None);
        let (_, hb, _) = reg.register("b".into(), None, None);
        reg.mark_online(&ha);
        reg.mark_online(&hb);

        // Remove a; b should be picked as the new default.
        let new_default = reg.pick_default_after_remove(&ha);
        assert_eq!(
            new_default.as_deref(),
            Some(hb.as_str()),
            "another online client should be picked"
        );
    }

    #[test]
    fn pick_default_after_remove_falls_back_to_offline_client() {
        let mut reg = ClientRegistry::new();
        let (_, ha, _) = reg.register("a".into(), None, None);
        let (_, hb, _) = reg.register("b".into(), None, None);
        // Neither is online; removing a should still pick b (from all_clients).
        let new_default = reg.pick_default_after_remove(&ha);
        assert_eq!(
            new_default.as_deref(),
            Some(hb.as_str()),
            "falls back to offline client when no online clients remain"
        );
    }

    #[test]
    fn pick_default_after_remove_returns_none_when_no_other_client() {
        let mut reg = ClientRegistry::new();
        let (_, ha, _) = reg.register("a".into(), None, None);

        // Only one client exists; after removing it there is no alternative.
        let new_default = reg.pick_default_after_remove(&ha);
        assert_eq!(
            new_default, None,
            "must return None when no other client exists"
        );
    }

    #[test]
    fn pick_default_after_remove_skips_the_removed_token() {
        let mut reg = ClientRegistry::new();
        let (_, ha, _) = reg.register("a".into(), None, None);
        reg.mark_online(&ha);

        // Only client a exists and is online; removing it must not pick itself.
        let new_default = reg.pick_default_after_remove(&ha);
        assert_eq!(
            new_default, None,
            "the removed token must not be picked as its own replacement"
        );
    }

    // ── allowlist-filtered resolution ───────────────────────────────────

    #[test]
    fn get_by_alias_in_rejects_hidden_name() {
        let mut reg = ClientRegistry::new();
        reg.register("claude".into(), None, None);
        reg.register("codex".into(), None, None);
        let allowed: std::collections::HashSet<String> = ["claude".to_string()].into();

        assert_eq!(
            reg.get_by_alias_in("claude", Some(&allowed))
                .map(|c| c.name.as_str()),
            Some("claude")
        );
        assert!(
            reg.get_by_alias_in("codex", Some(&allowed)).is_none(),
            "a hidden client must not resolve by exact name"
        );
    }

    #[test]
    fn get_by_alias_in_numbers_over_visible_set_only() {
        let mut reg = ClientRegistry::new();
        reg.register("charlie".into(), None, None);
        reg.register("alpha".into(), None, None);
        reg.register("bravo".into(), None, None);
        let allowed: std::collections::HashSet<String> =
            ["charlie".to_string(), "bravo".to_string()].into();

        // Visible sorted set is [bravo, charlie] → 1 = bravo, 2 = charlie.
        assert_eq!(
            reg.get_by_alias_in("1", Some(&allowed))
                .map(|c| c.name.as_str()),
            Some("bravo")
        );
        assert_eq!(
            reg.get_by_alias_in("2", Some(&allowed))
                .map(|c| c.name.as_str()),
            Some("charlie"),
            "index must not skip to a hidden client"
        );
        assert!(reg.get_by_alias_in("3", Some(&allowed)).is_none());
    }

    #[test]
    fn all_clients_in_filters_by_name() {
        let mut reg = ClientRegistry::new();
        reg.register("claude".into(), None, None);
        reg.register("codex".into(), None, None);
        let allowed: std::collections::HashSet<String> = ["claude".to_string()].into();

        let names: Vec<&str> = reg
            .all_clients_in(Some(&allowed))
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["claude"]);
        assert_eq!(reg.all_clients_in(None).len(), 2);
    }

    // ── rotate_vtoken ───────────────────────────────────────────────────

    #[test]
    fn rotate_vtoken_replaces_hash_and_preserves_metadata() {
        let mut reg = ClientRegistry::new();
        let (_old_plain, old_hash, _) = reg.register(
            "agent".into(),
            Some("label".into()),
            Some("description".into()),
        );
        reg.update_metadata(
            &old_hash,
            None,
            None,
            Some("Aria".into()),
            Some("🤖".into()),
        );
        reg.mark_online(&old_hash);
        let registered_at = reg.get_by_name("agent").unwrap().registered_at;

        let (new_plain, new_hash) = reg.rotate_vtoken("agent").expect("rotate");
        assert!(new_plain.starts_with("vhub_"), "plaintext must be returned");
        assert_eq!(new_hash, crate::hub::hash_vtoken(&new_plain));
        assert_ne!(new_hash, old_hash);

        assert!(
            reg.get_by_vtoken(&old_hash).is_none(),
            "the old hash must no longer resolve"
        );
        let c = reg.get_by_name("agent").expect("client survives rotation");
        assert_eq!(c.vtoken, new_hash);
        assert_eq!(c.label.as_deref(), Some("label"));
        assert_eq!(c.description.as_deref(), Some("description"));
        assert_eq!(c.persona_name.as_deref(), Some("Aria"));
        assert_eq!(c.persona_emoji.as_deref(), Some("🤖"));
        assert_eq!(c.registered_at, registered_at);
        assert!(!c.online, "rotated client must be marked offline");
    }

    #[test]
    fn rotate_vtoken_unknown_name_returns_none() {
        let mut reg = ClientRegistry::new();
        reg.register("agent".into(), None, None);
        assert!(reg.rotate_vtoken("nope").is_none());
        assert!(reg.get_by_name("agent").is_some());
    }
}
