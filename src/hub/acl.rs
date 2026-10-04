//! Agent access control: who may call whom.
//!
//! The Hub has two audiences that need an allowlist:
//!
//! - **A2A** (`call_agent` / `list_agents`): a calling Agent must be explicitly
//!   authorized for the target. **Default deny** — an unconfigured Hub rejects
//!   every A2A call (configure `*->*` or precise edges to opt back in).
//! - **WeChat** (`/list`, `/use`, `@name`, `/broadcast`): which backends a given
//!   WeChat user may see and select. When no WeChat entry (`user:<uid>->B` or a
//!   bare `B`) is configured the Hub stays unrestricted, preserving the
//!   single-tenant default.
//!
//! Configuration is a comma-separated env var ([`ENV_AGENT_ALLOWLIST`]):
//!
//! ```text
//! caller->target        A2A edge ("*" on either side is a wildcard)
//! user:<uid>->backend   that WeChat user sees / may use `backend`
//! backend               every WeChat user sees / may use `backend`
//! ```
//!
//! Malformed entries are logged and skipped: a typo narrows access instead of
//! silently widening it.

use std::collections::{HashMap, HashSet};

use tracing::warn;

/// Environment variable holding the allowlist spec.
pub const ENV_AGENT_ALLOWLIST: &str = "ILINK_AGENT_ALLOWLIST";

/// Wildcard token accepted on either side of an A2A edge.
const WILDCARD: &str = "*";

/// Parsed allowlist. An empty instance denies all A2A calls and leaves WeChat
/// visibility unrestricted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentAcl {
    /// A2A edges: caller name (or `*`) → set of allowed target names (or `*`).
    a2a: HashMap<String, HashSet<String>>,
    /// WeChat user id → backends that user may see.
    wechat_users: HashMap<String, HashSet<String>>,
    /// Backends visible to every WeChat user.
    wechat_public: HashSet<String>,
    /// True when the spec carried at least one non-empty entry.
    spec_present: bool,
}

impl AgentAcl {
    /// Build the ACL from [`ENV_AGENT_ALLOWLIST`]; unset or empty means "no spec".
    pub fn from_env() -> Self {
        match std::env::var(ENV_AGENT_ALLOWLIST) {
            Ok(spec) if !spec.trim().is_empty() => Self::parse(&spec),
            _ => Self::default(),
        }
    }

    /// Parse a spec. Malformed entries are skipped with a warning (fail-closed):
    /// they never grant access.
    pub fn parse(spec: &str) -> Self {
        let mut acl = Self::default();
        for raw in spec.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            acl.spec_present = true;
            match entry.split_once("->") {
                Some((lhs, rhs)) => {
                    let (lhs, rhs) = (lhs.trim(), rhs.trim());
                    if lhs.is_empty() || rhs.is_empty() {
                        warn!(
                            entry,
                            "ignoring malformed allowlist entry (empty edge side)"
                        );
                        continue;
                    }
                    if let Some(uid) = lhs.strip_prefix("user:") {
                        let uid = uid.trim();
                        if uid.is_empty() || uid == WILDCARD || rhs == WILDCARD {
                            warn!(
                                entry,
                                "ignoring malformed allowlist entry (bad WeChat user edge)"
                            );
                            continue;
                        }
                        acl.wechat_users
                            .entry(uid.to_string())
                            .or_default()
                            .insert(rhs.to_string());
                    } else {
                        acl.a2a
                            .entry(lhs.to_string())
                            .or_default()
                            .insert(rhs.to_string());
                    }
                }
                None => {
                    if entry == WILDCARD {
                        // Bare `*`: full A2A. WeChat stays unrestricted, which is the
                        // same "everything visible" the entry asks for.
                        acl.a2a
                            .entry(WILDCARD.to_string())
                            .or_default()
                            .insert(WILDCARD.to_string());
                    } else if let Some(_uid) = entry.strip_prefix("user:") {
                        warn!(
                            entry,
                            "ignoring malformed allowlist entry (WeChat entry needs `->backend`)"
                        );
                    } else {
                        acl.wechat_public.insert(entry.to_string());
                    }
                }
            }
        }
        acl
    }

    /// True when the spec carried at least one entry (even an unusable one).
    pub fn is_configured(&self) -> bool {
        self.spec_present
    }

    /// Number of configured A2A edges (for startup logging).
    pub fn a2a_edge_count(&self) -> usize {
        self.a2a.values().map(HashSet::len).sum()
    }

    /// Number of configured WeChat entries (for startup logging).
    pub fn wechat_entry_count(&self) -> usize {
        self.wechat_public.len() + self.wechat_users.values().map(HashSet::len).sum::<usize>()
    }

    /// May `caller` invoke `target` over A2A? Default deny: any caller/target
    /// pair without a matching edge (or `*` wildcard) is refused.
    pub fn allows_a2a(&self, caller: &str, target: &str) -> bool {
        let grants =
            |targets: &HashSet<String>| targets.contains(target) || targets.contains(WILDCARD);
        self.a2a.get(caller).is_some_and(grants) || self.a2a.get(WILDCARD).is_some_and(grants)
    }

    /// Targets `caller` may see and invoke over A2A.
    ///
    /// `None` means "unrestricted" (a `*` edge applies); `Some(set)` is the
    /// explicit allowlist — an empty set means the caller may invoke nobody.
    pub fn visible_targets_for_agent(&self, caller: &str) -> Option<HashSet<String>> {
        let mut visible = HashSet::new();
        for key in [caller, WILDCARD] {
            if let Some(targets) = self.a2a.get(key) {
                for target in targets {
                    if target == WILDCARD {
                        return None;
                    }
                    visible.insert(target.clone());
                }
            }
        }
        Some(visible)
    }

    /// Backends `user_id` may see and select from WeChat.
    ///
    /// `None` means "unrestricted" — no WeChat entry (`user:<uid>->B` or bare
    /// `B`) is configured. `Some(set)` is the resolved allowlist, filtered to
    /// `registered` names so callers can index into it directly; it may be empty.
    pub fn wechat_visible(&self, user_id: &str, registered: &[String]) -> Option<HashSet<String>> {
        if self.wechat_public.is_empty() && self.wechat_users.is_empty() {
            return None;
        }
        let mut visible = self.wechat_public.clone();
        if let Some(per_user) = self.wechat_users.get(user_id) {
            visible.extend(per_user.iter().cloned());
        }
        visible.retain(|name| registered.iter().any(|r| r == name));
        Some(visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a2a_edge_allows_only_the_configured_direction() {
        let acl = AgentAcl::parse("caller->target");
        assert!(acl.allows_a2a("caller", "target"));
        assert!(!acl.allows_a2a("target", "caller"), "edges are directional");
        assert!(!acl.allows_a2a("other", "target"));
        assert!(!acl.allows_a2a("caller", "other"));
    }

    #[test]
    fn a2a_defaults_to_deny() {
        for spec in ["", "user:user1->claude"] {
            let acl = AgentAcl::parse(spec);
            assert!(
                !acl.allows_a2a("caller", "target"),
                "spec {spec:?} must not grant A2A access"
            );
            assert_eq!(
                acl.visible_targets_for_agent("caller"),
                Some(HashSet::new()),
                "spec {spec:?} must expose no A2A target"
            );
        }
    }

    #[test]
    fn a2a_wildcards_widen_the_edge() {
        let all = AgentAcl::parse("*->*");
        assert!(all.allows_a2a("anyone", "anything"));
        assert_eq!(all.visible_targets_for_agent("anyone"), None);

        let target_side = AgentAcl::parse("caller->*");
        assert!(target_side.allows_a2a("caller", "anything"));
        assert!(!target_side.allows_a2a("other", "anything"));
        assert_eq!(target_side.visible_targets_for_agent("caller"), None);

        let caller_side = AgentAcl::parse("*->target");
        assert!(caller_side.allows_a2a("anyone", "target"));
        assert!(!caller_side.allows_a2a("anyone", "other"));
        assert_eq!(
            caller_side.visible_targets_for_agent("anyone"),
            Some(HashSet::from(["target".to_string()]))
        );
    }

    #[test]
    fn bare_star_allows_all_a2a() {
        let acl = AgentAcl::parse("*");
        assert!(acl.allows_a2a("caller", "target"));
        assert!(acl.is_configured());
    }

    #[test]
    fn visible_targets_lists_exactly_the_configured_edges() {
        let acl = AgentAcl::parse("caller->alpha,caller->bravo,other->charlie");
        assert_eq!(
            acl.visible_targets_for_agent("caller"),
            Some(HashSet::from(["alpha".to_string(), "bravo".to_string()]))
        );
        assert_eq!(
            acl.visible_targets_for_agent("other"),
            Some(HashSet::from(["charlie".to_string()]))
        );
    }

    #[test]
    fn malformed_entries_are_skipped_fail_closed() {
        let acl = AgentAcl::parse("->target,caller->,user:->backend,user:u1->,user:u2->*,,");
        assert!(!acl.allows_a2a("", "target"));
        assert!(!acl.allows_a2a("caller", ""));
        assert_eq!(acl.a2a_edge_count(), 0);
        assert_eq!(acl.wechat_entry_count(), 0);
        assert!(acl.is_configured(), "entries were present, just unusable");
    }

    #[test]
    fn wechat_unrestricted_when_no_wechat_entry_is_configured() {
        for spec in ["", "caller->target", "*->*", "*"] {
            let acl = AgentAcl::parse(spec);
            assert_eq!(
                acl.wechat_visible("user1", &names(&["claude"])),
                None,
                "spec {spec:?} must leave WeChat visibility unrestricted"
            );
        }
    }

    #[test]
    fn wechat_user_edge_grants_only_that_user() {
        let acl = AgentAcl::parse("user:user1->claude");
        let registered = names(&["claude", "codex"]);
        assert_eq!(
            acl.wechat_visible("user1", &registered),
            Some(HashSet::from(["claude".to_string()]))
        );
        assert_eq!(
            acl.wechat_visible("user2", &registered),
            Some(HashSet::new()),
            "an unconfigured user sees nothing"
        );
    }

    #[test]
    fn wechat_public_entry_is_visible_to_every_user() {
        let acl = AgentAcl::parse("claude,user:user1->codex");
        let registered = names(&["claude", "codex", "other"]);
        assert_eq!(
            acl.wechat_visible("user1", &registered),
            Some(HashSet::from(["claude".to_string(), "codex".to_string()]))
        );
        assert_eq!(
            acl.wechat_visible("user2", &registered),
            Some(HashSet::from(["claude".to_string()]))
        );
        assert_eq!(acl.wechat_entry_count(), 2);
    }

    #[test]
    fn wechat_visible_drops_unregistered_names() {
        let acl = AgentAcl::parse("claude");
        assert_eq!(
            acl.wechat_visible("user1", &names(&["codex"])),
            Some(HashSet::new()),
            "a name that is not registered resolves to nothing"
        );
    }
}
