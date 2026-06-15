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
    /// Optional: if omitted, routing defaults to `fixed` with the profile named
    /// `claude` (if present), then `default`, then the first alphabetically.
    #[serde(default)]
    pub routing: Option<BridgeRoutingYaml>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StdinMode {
    #[default]
    None,
    Message,
}

/// Per-profile CLI settings (multi-profile YAML) or the only profile (legacy single file).
///
/// **`type` shorthand**: set `type: claude-code` to use a built-in profile.
///
/// **`script` shorthand**: set `script: ./my-handler.py` (or `.js`, `.sh`, `.ts`, `.rb`) and
/// bridge infers the runtime automatically:
/// - `.py`  → `python3 <script>`
/// - `.js` / `.mjs` → `node <script>`
/// - `.ts`  → `npx tsx <script>`
/// - `.sh`  → `bash <script>`
/// - `.rb`  → `ruby <script>`
/// - other  → execute directly (must be chmod +x)
///
/// An explicit `command` always wins over `type` / `script`.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeProfile {
    /// Optional built-in type shorthand (e.g. `"claude-code"`).
    /// When set and `command` is empty, the profile is expanded to the corresponding built-in.
    #[serde(default, rename = "type")]
    pub profile_type: Option<String>,

    /// Path to a script file. Bridge infers the runtime from the file extension.
    /// Expanded to `command` + `args` at load time. An explicit `command` takes priority.
    #[serde(default)]
    pub script: Option<String>,

    #[serde(default)]
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
    /// If non-empty: stdout 的第一行若以该前缀开头，则该行去掉前缀后的剩余部分为 **CLI 会话 id**，
    /// 会随 `sendmessage` 写入 Hub；其余行作为发给微信的正文。
    #[serde(default)]
    pub cli_session_first_line_prefix: Option<String>,
}

impl Default for BridgeProfile {
    fn default() -> Self {
        Self {
            profile_type: None,
            script: None,
            command: String::new(),
            args: Vec::new(),
            stdin: StdinMode::default(),
            cwd: None,
            env: HashMap::new(),
            timeout_secs: default_timeout_secs(),
            max_reply_chars: default_max_reply_chars(),
            truncation_suffix: default_truncation_suffix(),
            include_stderr_in_reply: false,
            cli_session_first_line_prefix: None,
        }
    }
}

fn default_timeout_secs() -> u64 {
    1800
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
    #[serde(default)]
    pub cli_session_first_line_prefix: Option<String>,
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
            serde_norway::from_str(raw).context("serde_norway::from_str BridgeFileRaw")?;
        match file {
            BridgeFileRaw::Single(c) => Self::from_single(c),
            BridgeFileRaw::Multi(m) => Self::from_multi(m),
        }
    }

    fn from_single(c: BridgeConfig) -> Result<Self> {
        c.validate()?;
        let profile = BridgeProfile {
            profile_type: None,
            script: None,
            command: c.command.clone(),
            args: c.args.clone(),
            stdin: c.stdin.clone(),
            cwd: c.cwd.clone(),
            env: c.env.clone(),
            timeout_secs: c.timeout_secs,
            max_reply_chars: c.max_reply_chars,
            truncation_suffix: c.truncation_suffix.clone(),
            include_stderr_in_reply: c.include_stderr_in_reply,
            cli_session_first_line_prefix: c.cli_session_first_line_prefix.clone(),
        };
        warn_shell_injection_risk(&profile, "default");
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), profile);
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
        // Expand script/type shortcuts before validation so `command` is filled in.
        // Order matters: script → type (explicit command wins over both).
        let profiles: HashMap<String, BridgeProfile> = m
            .profiles
            .into_iter()
            .map(|(name, p)| {
                let expanded = expand_script_field(p, &name)?;
                let expanded = expand_profile_type(expanded, &name)?;
                Ok((name, expanded))
            })
            .collect::<Result<_>>()?;

        for (name, p) in &profiles {
            if p.command.trim().is_empty() {
                anyhow::bail!(
                    "profile `{name}`: `command` must not be empty \
                     (set `command`, `script`, or use a recognized `type`)"
                );
            }
            warn_shell_injection_risk(p, name);
        }

        // Resolve routing: if omitted, auto-detect fixed routing using a sensible default.
        let routing_cfg = m.routing.unwrap_or_else(|| {
            let default = if profiles.contains_key("claude") {
                "claude".to_string()
            } else if profiles.contains_key("default") {
                "default".to_string()
            } else {
                let mut keys: Vec<&String> = profiles.keys().collect();
                keys.sort();
                keys[0].clone()
            };
            BridgeRoutingYaml {
                strategy: RoutingStrategy::Fixed,
                default_profile: default,
                prefix_rules: vec![],
            }
        });

        if !profiles.contains_key(&routing_cfg.default_profile) {
            anyhow::bail!(
                "routing.default_profile `{}` is not a key in `profiles`",
                routing_cfg.default_profile
            );
        }
        for (i, rule) in routing_cfg.prefix_rules.iter().enumerate() {
            if rule.prefix.is_empty() {
                anyhow::bail!("routing.prefix_rules[{i}]: `prefix` must not be empty");
            }
            if !profiles.contains_key(&rule.profile) {
                anyhow::bail!(
                    "routing.prefix_rules[{i}]: unknown profile `{}`",
                    rule.profile
                );
            }
        }
        if routing_cfg.strategy == RoutingStrategy::Prefix && routing_cfg.prefix_rules.is_empty() {
            anyhow::bail!("routing.strategy: `prefix` requires at least one `prefix_rules` entry (or use `fixed`)");
        }

        let routing = match routing_cfg.strategy {
            RoutingStrategy::Fixed => RoutingState::Fixed(routing_cfg.default_profile.clone()),
            RoutingStrategy::Prefix => RoutingState::Prefix {
                default: routing_cfg.default_profile.clone(),
                rules: routing_cfg
                    .prefix_rules
                    .iter()
                    .map(|r| (r.prefix.clone(), r.profile.clone()))
                    .collect(),
            },
        };

        Ok(Self {
            profiles,
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

    pub fn profile(&self, name: &str) -> Option<&BridgeProfile> {
        self.profiles.get(name)
    }

    pub fn default_profile_name(&self) -> &str {
        match &self.routing {
            RoutingState::Fixed(name) => name,
            RoutingState::Prefix { default, .. } => default,
        }
    }

    pub fn routing_label(&self) -> &'static str {
        match &self.routing {
            RoutingState::Fixed(_) => "fixed",
            RoutingState::Prefix { .. } => "prefix",
        }
    }
}

/// Expand a `script: <path>` field to `command` + `args` based on file extension.
///
/// | Extension            | Inferred runtime              |
/// |----------------------|-------------------------------|
/// | `.py`                | `python3 <script>`            |
/// | `.js` / `.mjs`       | `node <script>`               |
/// | `.ts`                | `npx tsx <script>`            |
/// | `.sh` / `.bash`      | `bash <script>`               |
/// | `.rb`                | `ruby <script>`               |
/// | other / no extension | execute directly (chmod +x)   |
///
/// If `command` is already set, returns the profile unchanged (explicit wins).
fn expand_script_field(mut p: BridgeProfile, name: &str) -> Result<BridgeProfile> {
    let Some(ref script) = p.script.clone() else {
        return Ok(p);
    };
    if !p.command.trim().is_empty() {
        // Explicit command wins; script field is informational only.
        return Ok(p);
    }
    let script_path = script.trim().to_string();
    let ext = std::path::Path::new(&script_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "py" => {
            p.command = "python3".to_string();
            let mut args = vec![script_path];
            args.append(&mut p.args);
            p.args = args;
        }
        "js" | "mjs" | "cjs" => {
            p.command = "node".to_string();
            let mut args = vec![script_path];
            args.append(&mut p.args);
            p.args = args;
        }
        "ts" => {
            p.command = "npx".to_string();
            let mut args = vec!["tsx".to_string(), script_path];
            args.append(&mut p.args);
            p.args = args;
        }
        "sh" | "bash" => {
            p.command = "bash".to_string();
            let mut args = vec![script_path];
            args.append(&mut p.args);
            p.args = args;
        }
        "rb" => {
            p.command = "ruby".to_string();
            let mut args = vec![script_path];
            args.append(&mut p.args);
            p.args = args;
        }
        _ => {
            // No known extension: run as executable (requires chmod +x / shebang).
            p.command = script_path;
        }
    }
    tracing::debug!(
        profile = name,
        command = %p.command,
        "script: field expanded"
    );
    Ok(p)
}

/// Expand a `type: <shorthand>` profile into a full exec-mode profile.
///
/// Recognised shorthands:
/// - `"claude-code"` → `command: ilink-hub-bridge  args: [profile, claude-code]`
///   with `cli_session_first_line_prefix: "ILINK_SESSION:"` auto-set.
/// - `"cursor"` → `command: ilink-hub-bridge  args: [profile, cursor]`
///   with `cli_session_first_line_prefix: "ILINK_SESSION:"` auto-set.
///
/// If `profile_type` is `None` or the command is already set, returns the profile unchanged.
fn expand_profile_type(mut p: BridgeProfile, name: &str) -> Result<BridgeProfile> {
    let Some(ref pt) = p.profile_type.clone() else {
        return Ok(p);
    };
    if !p.command.trim().is_empty() {
        // Explicit command wins; type is informational only.
        return Ok(p);
    }
    match pt.as_str() {
        "claude-code" => {
            p.command = "ilink-hub-bridge".to_string();
            p.args = vec!["profile".to_string(), "claude-code".to_string()];
            p.stdin = StdinMode::Message;
            if p.cli_session_first_line_prefix.is_none() {
                p.cli_session_first_line_prefix = Some("ILINK_SESSION:".to_string());
            }
            Ok(p)
        }
        "cursor" => {
            p.command = "ilink-hub-bridge".to_string();
            p.args = vec!["profile".to_string(), "cursor".to_string()];
            p.stdin = StdinMode::Message;
            if p.cli_session_first_line_prefix.is_none() {
                p.cli_session_first_line_prefix = Some("ILINK_SESSION:".to_string());
            }
            Ok(p)
        }
        "codex" => {
            p.command = "ilink-hub-bridge".to_string();
            p.args = vec!["profile".to_string(), "codex".to_string()];
            p.stdin = StdinMode::Message;
            if p.cli_session_first_line_prefix.is_none() {
                p.cli_session_first_line_prefix = Some("ILINK_SESSION:".to_string());
            }
            Ok(p)
        }
        "agy" => {
            p.command = "ilink-hub-bridge".to_string();
            p.args = vec!["profile".to_string(), "agy".to_string()];
            p.stdin = StdinMode::Message;
            if p.cli_session_first_line_prefix.is_none() {
                p.cli_session_first_line_prefix = Some("ILINK_SESSION:".to_string());
            }
            Ok(p)
        }
        other => anyhow::bail!(
            "profile `{name}`: unknown `type: {other}`; supported built-in types: claude-code, cursor, codex, agy"
        ),
    }
}

/// Warn when a profile has a dangerous shell-injection pattern:
/// a shell interpreter (bash/sh/zsh) as the command with `-c` as an arg AND
/// `{{MESSAGE}}` somewhere in the args — user input would be interpolated into
/// a shell command string, enabling arbitrary command execution.
///
/// `tokio::process::Command` does NOT invoke a shell automatically, so this is
/// only dangerous when the user explicitly invokes a shell with `-c`.
/// Recommendation: use `stdin: message` instead.
fn warn_shell_injection_risk(p: &BridgeProfile, name: &str) {
    const SHELL_CMDS: &[&str] = &["bash", "sh", "zsh", "fish", "dash"];
    let cmd = std::path::Path::new(&p.command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p.command);
    if !SHELL_CMDS.contains(&cmd) {
        return;
    }
    let has_dash_c = p.args.iter().any(|a| a == "-c");
    let has_message_placeholder = p.args.iter().any(|a| a.contains("{{MESSAGE}}"));
    if has_dash_c && has_message_placeholder {
        tracing::warn!(
            profile = name,
            command = %p.command,
            "SECURITY: profile uses a shell with `-c` and `{{{{MESSAGE}}}}` in args — \
             user input will be interpolated into a shell command string. \
             Use `stdin: message` to pass the message safely via stdin instead."
        );
    }
}

/// Expand `${VAR}` placeholders in `template` using values from `env`.
///
/// Rules:
/// - `${IDENT}` → value of `IDENT` from `env`; error if not found (even empty string is ok)
/// - `$$` → literal `$`
/// - No other `$...` forms are recognised; they pass through unchanged
/// - Invalid tokens like `${}` or `${1FOO}` are errors
///
/// Only exercised by unit tests today; the production path calls
/// [`expand_env_var_named`] directly.
#[allow(dead_code)]
pub fn expand_env_var(
    template: &str,
    env: &std::collections::HashMap<String, String>,
) -> Result<String> {
    expand_env_var_named(template, env, None, None)
}

/// Same as [`expand_env_var`] but includes profile/field context in error messages.
pub fn expand_env_var_named(
    template: &str,
    env: &std::collections::HashMap<String, String>,
    profile: Option<&str>,
    field: Option<&str>,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        // We have a `$`
        if i + 1 >= len {
            // Trailing `$` with nothing after — pass through
            out.push('$');
            i += 1;
            continue;
        }

        match bytes[i + 1] {
            b'$' => {
                // `$$` → literal `$`
                out.push('$');
                i += 2;
            }
            b'{' => {
                // Find closing `}`
                let start = i + 2;
                let end = match template[start..].find('}') {
                    Some(rel) => start + rel,
                    None => {
                        anyhow::bail!(
                            "{}unclosed `${{` in env template: {:?}",
                            location_prefix(profile, field),
                            template
                        );
                    }
                };
                let ident = &template[start..end];
                // Validate identifier: must match [A-Za-z_][A-Za-z0-9_]*
                validate_env_ident(ident, template, profile, field)?;
                let value = env.get(ident).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}env var `{}` not found (referenced in template {:?})",
                        location_prefix(profile, field),
                        ident,
                        template
                    )
                })?;
                out.push_str(value);
                i = end + 1;
            }
            _ => {
                // Plain `$x` — not a recognised form, pass through unchanged
                out.push('$');
                i += 1;
            }
        }
    }

    Ok(out)
}

fn validate_env_ident(
    ident: &str,
    template: &str,
    profile: Option<&str>,
    field: Option<&str>,
) -> Result<()> {
    let mut chars = ident.chars();
    let first = chars.next().ok_or_else(|| {
        anyhow::anyhow!(
            "{}empty identifier in `${{}}` in env template: {:?}",
            location_prefix(profile, field),
            template
        )
    })?;
    if !first.is_ascii_alphabetic() && first != '_' {
        anyhow::bail!(
            "{}invalid env var name `{}` in template {:?}: must start with [A-Za-z_]",
            location_prefix(profile, field),
            ident,
            template
        );
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            anyhow::bail!(
                "{}invalid env var name `{}` in template {:?}: only [A-Za-z0-9_] allowed",
                location_prefix(profile, field),
                ident,
                template
            );
        }
    }
    Ok(())
}

fn location_prefix(profile: Option<&str>, field: Option<&str>) -> String {
    match (profile, field) {
        (Some(p), Some(f)) => format!("profile `{p}`, field `{f}`: "),
        (Some(p), None) => format!("profile `{p}`: "),
        _ => String::new(),
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
    fn script_field_py_expands_to_python3() {
        let y = r#"
profiles:
  bot:
    script: ./my_handler.py
    timeout_secs: 60
routing:
  strategy: fixed
  default_profile: bot
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (_, p, _) = app.resolve("hello").unwrap();
        assert_eq!(p.command, "python3");
        assert_eq!(p.args, vec!["./my_handler.py"]);
    }

    #[test]
    fn script_field_js_expands_to_node() {
        let y = r#"
profiles:
  bot:
    script: ./handler.js
routing:
  strategy: fixed
  default_profile: bot
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "node");
        assert_eq!(p.args, vec!["./handler.js"]);
    }

    #[test]
    fn script_field_ts_expands_to_npx_tsx() {
        let y = r#"
profiles:
  bot:
    script: ./handler.ts
routing:
  strategy: fixed
  default_profile: bot
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "npx");
        assert_eq!(p.args, vec!["tsx", "./handler.ts"]);
    }

    #[test]
    fn script_field_sh_expands_to_bash() {
        let y = r#"
profiles:
  bot:
    script: ./run.sh
routing:
  strategy: fixed
  default_profile: bot
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "bash");
        assert_eq!(p.args, vec!["./run.sh"]);
    }

    #[test]
    fn explicit_command_wins_over_script() {
        let y = r#"
profiles:
  bot:
    script: ./handler.py
    command: /usr/bin/python3.11
    args: ["./handler.py"]
routing:
  strategy: fixed
  default_profile: bot
"#;
        let app = BridgeApp::parse_yaml(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "/usr/bin/python3.11");
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

    // ── expand_env_var ────────────────────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expand_simple_substitution() {
        let e = env(&[("FOO", "bar")]);
        assert_eq!(expand_env_var("${FOO}", &e).unwrap(), "bar");
    }

    #[test]
    fn expand_multiple_occurrences() {
        let e = env(&[("X", "hello")]);
        assert_eq!(
            expand_env_var("${X} and ${X}", &e).unwrap(),
            "hello and hello"
        );
    }

    #[test]
    fn expand_multiple_different_vars() {
        let e = env(&[("USER", "alice"), ("KEY_SUFFIX", "abc123")]);
        assert_eq!(
            expand_env_var("hello ${USER}, your key ends in ${KEY_SUFFIX}", &e).unwrap(),
            "hello alice, your key ends in abc123"
        );
    }

    #[test]
    fn expand_double_dollar_escape() {
        let e = env(&[]);
        assert_eq!(expand_env_var("$$HOME", &e).unwrap(), "$HOME");
        assert_eq!(expand_env_var("price is $$5", &e).unwrap(), "price is $5");
    }

    #[test]
    fn expand_mixed_literal_and_var() {
        let e = env(&[("KEY", "sk-123")]);
        assert_eq!(
            expand_env_var("prefix-${KEY}-suffix", &e).unwrap(),
            "prefix-sk-123-suffix"
        );
    }

    #[test]
    fn expand_no_placeholder_passthrough() {
        let e = env(&[]);
        assert_eq!(expand_env_var("plain string", &e).unwrap(), "plain string");
        assert_eq!(expand_env_var("", &e).unwrap(), "");
    }

    #[test]
    fn expand_empty_value_is_ok() {
        let e = env(&[("EMPTY", "")]);
        assert_eq!(
            expand_env_var("before${EMPTY}after", &e).unwrap(),
            "beforeafter"
        );
    }

    #[test]
    fn expand_missing_var_errors() {
        let e = env(&[]);
        let err = expand_env_var("${MISSING}", &e).unwrap_err();
        assert!(err.to_string().contains("MISSING"));
    }

    #[test]
    fn expand_missing_var_error_includes_profile_and_field() {
        let e = env(&[]);
        let err =
            expand_env_var_named("${X}", &e, Some("myprofile"), Some("env.ANTHROPIC_API_KEY"))
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("myprofile"));
        assert!(msg.contains("env.ANTHROPIC_API_KEY"));
        assert!(msg.contains("X"));
    }

    #[test]
    fn expand_invalid_empty_ident_errors() {
        let e = env(&[]);
        assert!(expand_env_var("${}", &e).is_err());
    }

    #[test]
    fn expand_invalid_leading_digit_errors() {
        let e = env(&[]);
        assert!(expand_env_var("${1FOO}", &e).is_err());
    }

    #[test]
    fn expand_invalid_space_in_ident_errors() {
        let e = env(&[]);
        assert!(expand_env_var("${VAR with space}", &e).is_err());
    }

    #[test]
    fn expand_unclosed_brace_errors() {
        let e = env(&[]);
        assert!(expand_env_var("${UNCLOSED", &e).is_err());
    }

    #[test]
    fn expand_plain_dollar_passthrough() {
        // `$x` (no braces) is not a recognised form — pass through unchanged
        let e = env(&[]);
        assert_eq!(expand_env_var("$HOME", &e).unwrap(), "$HOME");
    }

    #[test]
    fn expand_trailing_dollar_passthrough() {
        let e = env(&[]);
        assert_eq!(expand_env_var("end$", &e).unwrap(), "end$");
    }
}
