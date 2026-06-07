//! Bridge YAML: single-profile (legacy) or multi-profile with routing.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// How to pick a profile for each inbound text message (multi-profile YAML only).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Always run `default_profile`; inbound text is passed unchanged to `{{MESSAGE}}`.
    #[default]
    Fixed,
    /// First matching `prefix_rules` wins (order matters; put longer prefixes first).
    /// The matched prefix is stripped from the string used for `{{MESSAGE}}` / stdin.
    Prefix,
}

#[derive(Debug, Deserialize)]
pub struct PrefixRuleYaml {
    pub prefix: String,
    pub profile: String,
}

#[derive(Debug, Deserialize)]
pub struct BridgeRoutingYaml {
    #[serde(default)]
    pub strategy: RoutingStrategy,
    pub default_profile: String,
    #[serde(default)]
    pub prefix_rules: Vec<PrefixRuleYaml>,
}

#[derive(Debug, Deserialize)]
pub struct BridgeMultiYaml {
    #[serde(default = "default_true")]
    pub skip_bot_messages: bool,
    #[serde(default = "default_true")]
    pub require_text: bool,
    #[serde(default = "default_true")]
    pub send_error_reply: bool,
    pub profiles: HashMap<String, BridgeProfile>,
    pub routing: BridgeRoutingYaml,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StdinMode {
    #[default]
    None,
    Message,
}

/// Per-profile CLI settings (multi-profile YAML) or the only profile (legacy single file).
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeProfile {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: StdinMode,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_reply_chars")]
    pub max_reply_chars: usize,
    #[serde(default = "default_truncation_suffix")]
    pub truncation_suffix: String,
    #[serde(default)]
    pub include_stderr_in_reply: bool,
}

fn default_timeout_secs() -> u64 {
    300
}

fn default_max_reply_chars() -> usize {
    8000
}

fn default_truncation_suffix() -> String {
    "\n\n…(输出已截断)".to_string()
}

fn default_true() -> bool {
    true
}

/// Legacy flat YAML (one `command`, optional global flags).
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: StdinMode,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_reply_chars")]
    pub max_reply_chars: usize,
    #[serde(default = "default_truncation_suffix")]
    pub truncation_suffix: String,
    #[serde(default = "default_true")]
    pub skip_bot_messages: bool,
    #[serde(default = "default_true")]
    pub require_text: bool,
    #[serde(default = "default_true")]
    pub send_error_reply: bool,
    #[serde(default)]
    pub include_stderr_in_reply: bool,
}

impl BridgeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            anyhow::bail!("`command` must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BridgeFileRaw {
    /// Must contain top-level `profiles` + `routing`.
    Multi(BridgeMultiYaml),
    Single(BridgeConfig),
}

#[derive(Debug, Clone)]
enum RoutingState {
    Fixed(String),
    Prefix {
        default: String,
        rules: Vec<(String, String)>,
    },
}

/// Loaded bridge configuration: either migrated from a single flat file or from multi-profile YAML.
#[derive(Debug, Clone)]
pub struct BridgeApp {
    profiles: HashMap<String, BridgeProfile>,
    routing: RoutingState,
    pub skip_bot_messages: bool,
    pub require_text: bool,
    pub send_error_reply: bool,
}

impl BridgeApp {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse_yaml(&raw).with_context(|| format!("parse YAML {}", path.display()))
    }

    /// Parse YAML from a string (same as file body). Used by tests and for tooling.
    pub fn parse_yaml(raw: &str) -> Result<Self> {
        let file: BridgeFileRaw =
            serde_yaml::from_str(raw).context("serde_yaml::from_str BridgeFileRaw")?;
        match file {
            BridgeFileRaw::Single(c) => Self::from_single(c),
            BridgeFileRaw::Multi(m) => Self::from_multi(m),
        }
    }

    fn from_single(c: BridgeConfig) -> Result<Self> {
        c.validate()?;
        let mut profiles = HashMap::new();
        profiles.insert(
            "default".to_string(),
            BridgeProfile {
                command: c.command.clone(),
                args: c.args.clone(),
                stdin: c.stdin.clone(),
                cwd: c.cwd.clone(),
                env: c.env.clone(),
                timeout_secs: c.timeout_secs,
                max_reply_chars: c.max_reply_chars,
                truncation_suffix: c.truncation_suffix.clone(),
                include_stderr_in_reply: c.include_stderr_in_reply,
            },
        );
        Ok(Self {
            profiles,
            routing: RoutingState::Fixed("default".to_string()),
            skip_bot_messages: c.skip_bot_messages,
            require_text: c.require_text,
            send_error_reply: c.send_error_reply,
        })
    }

    fn from_multi(m: BridgeMultiYaml) -> Result<Self> {
        if m.profiles.is_empty() {
            anyhow::bail!("`profiles` must contain at least one profile");
        }
        for (name, p) in &m.profiles {
            if p.command.trim().is_empty() {
                anyhow::bail!("profile `{name}`: `command` must not be empty");
            }
        }
        if !m.profiles.contains_key(&m.routing.default_profile) {
            anyhow::bail!(
                "routing.default_profile `{}` is not a key in `profiles`",
                m.routing.default_profile
            );
        }
        for (i, rule) in m.routing.prefix_rules.iter().enumerate() {
            if rule.prefix.is_empty() {
                anyhow::bail!("routing.prefix_rules[{i}]: `prefix` must not be empty");
            }
            if !m.profiles.contains_key(&rule.profile) {
                anyhow::bail!(
                    "routing.prefix_rules[{i}]: unknown profile `{}`",
                    rule.profile
                );
            }
        }
        if m.routing.strategy == RoutingStrategy::Prefix && m.routing.prefix_rules.is_empty() {
            anyhow::bail!("routing.strategy: `prefix` requires at least one `prefix_rules` entry (or use `fixed`)");
        }

        let routing = match m.routing.strategy {
            RoutingStrategy::Fixed => RoutingState::Fixed(m.routing.default_profile.clone()),
            RoutingStrategy::Prefix => RoutingState::Prefix {
                default: m.routing.default_profile.clone(),
                rules: m
                    .routing
                    .prefix_rules
                    .iter()
                    .map(|r| (r.prefix.clone(), r.profile.clone()))
                    .collect(),
            },
        };

        Ok(Self {
            profiles: m.profiles,
            routing,
            skip_bot_messages: m.skip_bot_messages,
            require_text: m.require_text,
            send_error_reply: m.send_error_reply,
        })
    }

    /// Pick profile and payload text for CLI (after Hub routing; `text` is usually `msg.text()`).
    pub fn resolve<'a>(&'a self, text: &str) -> Result<(&'a str, &'a BridgeProfile, String)> {
        match &self.routing {
            RoutingState::Fixed(name) => {
                let p = self
                    .profiles
                    .get(name)
                    .with_context(|| format!("internal: missing profile `{name}`"))?;
                Ok((name.as_str(), p, text.to_string()))
            }
            RoutingState::Prefix { default, rules } => {
                for (prefix, pname) in rules {
                    if text.starts_with(prefix) {
                        let p = self.profiles.get(pname).with_context(|| {
                            format!("internal: prefix rule references missing profile `{pname}`")
                        })?;
                        let rest = text[prefix.len()..].trim_start().to_string();
                        return Ok((pname.as_str(), p, rest));
                    }
                }
                let p = self
                    .profiles
                    .get(default)
                    .with_context(|| format!("internal: missing default profile `{default}`"))?;
                Ok((default.as_str(), p, text.to_string()))
            }
        }
    }

    pub fn profile_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.profiles.keys().map(|s| s.as_str()).collect();
        v.sort();
        v
    }

    pub fn routing_label(&self) -> &'static str {
        match &self.routing {
            RoutingState::Fixed(_) => "fixed",
            RoutingState::Prefix { .. } => "prefix",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_legacy_flat_yaml() {
        let y = r#"
command: echo
args: ["{{MESSAGE}}"]
stdin: none
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        assert_eq!(app.profile_names(), vec!["default"]);
        let (_, p, payload) = app.resolve("hello").unwrap();
        assert_eq!(p.command, "echo");
        assert_eq!(payload, "hello");
    }

    #[test]
    fn parse_multi_prefix_routing() {
        let y = r#"
profiles:
  a:
    command: echo
    args: ["A", "{{MESSAGE}}"]
    stdin: none
    timeout_secs: 5
  b:
    command: echo
    args: ["B", "{{MESSAGE}}"]
    stdin: none
    timeout_secs: 5
routing:
  strategy: prefix
  default_profile: a
  prefix_rules:
    - prefix: "/b "
      profile: b
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (n, _, pay) = app.resolve("plain").unwrap();
        assert_eq!(n, "a");
        assert_eq!(pay, "plain");

        let (n, _, pay) = app.resolve("/b hi").unwrap();
        assert_eq!(n, "b");
        assert_eq!(pay, "hi");
    }

    #[test]
    fn parse_multi_fixed_two_profiles() {
        let y = r#"
profiles:
  p1:
    command: echo
    args: ["1", "{{MESSAGE}}"]
    stdin: none
    timeout_secs: 3
  p2:
    command: echo
    args: ["2", "{{MESSAGE}}"]
    stdin: none
    timeout_secs: 3
routing:
  strategy: fixed
  default_profile: p2
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (n, _, pay) = app.resolve("/b hello").unwrap();
        assert_eq!(n, "p2");
        assert_eq!(pay, "/b hello");
    }

    #[test]
    fn multi_empty_profiles_errors() {
        let y = r#"
profiles: {}
routing:
  strategy: fixed
  default_profile: x
"#;
        assert!(BridgeApp::parse_yaml(y).is_err());
    }
}
