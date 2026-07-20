//! Bridge YAML: one file == one agentproc profile (hub form under `agentproc:`).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Which IM protocol the bridge speaks. Stage 2: only `ilink` is implemented;
/// any other string loads a `NullTransport` placeholder to prove the transport
/// seam is pluggable (real adapters land in later stages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportKind(String);

impl TransportKind {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// `ilink` is the only fully-implemented transport.
    pub fn is_ilink(&self) -> bool {
        self.0 == "ilink"
    }
}

impl Default for TransportKind {
    fn default() -> Self {
        Self("ilink".to_string())
    }
}

impl<'de> Deserialize<'de> for TransportKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self(s))
    }
}

/// How the bridge obtains credentials / where it points the transport.
/// - `hub` (default): resolve a virtual token via the Hub (`/hub/register` / QR).
/// - `direct`: connect straight to the real iLink upstream. Stage 2 only supports
///   `direct` with an explicit `WEIXIN_TOKEN` + `WEIXIN_BASE_URL`; the QR / auto-
///   register flow for direct lands in stage 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Via {
    #[default]
    Hub,
    Direct,
}

impl Via {
    pub fn is_hub(self) -> bool {
        matches!(self, Self::Hub)
    }
    pub fn is_direct(self) -> bool {
        matches!(self, Self::Direct)
    }
}

impl<'de> Deserialize<'de> for Via {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.trim().to_ascii_lowercase().as_str() {
            "hub" => Ok(Self::Hub),
            "direct" => Ok(Self::Direct),
            other => Err(serde::de::Error::custom(format!(
                "unknown `via: {other}` (expected `hub` or `direct`)"
            ))),
        }
    }
}

/// One bridge profile file == one agentproc profile, in spec-aligned hub form:
/// a pure agentproc execution config nested under `agentproc:`, with ilink-hub
/// metadata (`description`) and the `script:` shorthand as siblings.
///
/// ```yaml
/// description: issue-keeper on MiniMax
/// agentproc:
///   executor: claude-code
///   cwd: /path/to/project
///   streaming: false
///   env:
///     ANTHROPIC_API_KEY: ${MINIMAX_API_KEY}
///     CLAUDE_MODEL: MiniMax-M3
/// ```
#[derive(Debug, Deserialize)]
pub struct BridgeProfileFile {
    /// Agent description (surfaced via the Hub MCP `list_agents` tool).
    #[serde(default)]
    pub description: Option<String>,

    /// Optional `script: <path>` shorthand — expanded into `agentproc.command`/
    /// `args` by file extension. An explicit `agentproc.command` wins.
    #[serde(default)]
    pub script: Option<String>,

    /// The pure agentproc-spec execution config.
    #[serde(default)]
    pub agentproc: AgentprocBlock,

    /// Which IM protocol to speak. Default `ilink`; any other string loads a
    /// `NullTransport` placeholder (stage 2 pluggability proof; real adapters
    /// arrive in later stages).
    #[serde(default)]
    pub transport: TransportKind,

    /// Credential resolution / connection target. Default `hub` (resolve a
    /// virtual token via the Hub). `direct` connects to the real iLink upstream
    /// — stage 3 supports QR login against the real upstream, a saved direct
    /// credential file, or an explicit `WEIXIN_TOKEN`.
    #[serde(default)]
    pub via: Via,

    /// Base URL of the real iLink upstream for `via: direct`
    /// (e.g. `https://ilinkai.weixin.qq.com`). When set, overrides `--hub-url` /
    /// `WEIXIN_BASE_URL` for this profile — lets a bridge manager mix hub and
    /// direct profiles against different upstreams. Ignored when `via: hub`.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// The `agentproc:` block — field-for-field the agentproc profile spec
/// (`executor`, `command`, `args`, `cwd`, `env`, `env_allowlist`,
/// `timeout_secs`, `kill_grace_secs`, `max_reply_chars`, `truncation_suffix`,
/// `include_stderr_in_reply`, `send_error_reply`, `streaming`, `permission`).
/// Parsed into the flat [`BridgeProfile`] at load time.
#[derive(Debug, Default, Deserialize)]
pub struct AgentprocBlock {
    /// Optional in-process executor name (e.g. `claude-code`, `codex`,
    /// `cursor`, `codebuddy`, `agy`). When set and recognised by the agentproc
    /// SDK, the runner drives the CLI in-process; otherwise it falls back to
    /// spawning `command`/`args`.
    #[serde(default)]
    pub executor: Option<String>,

    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Restrict which `${VAR}` references the `env` block may expand against the
    /// bridge's environment. Absent = expand against the full environment.
    #[serde(default)]
    pub env_allowlist: Option<Vec<String>>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_kill_grace_secs")]
    pub kill_grace_secs: u64,
    #[serde(default = "default_max_reply_chars")]
    pub max_reply_chars: usize,
    #[serde(default = "default_truncation_suffix")]
    pub truncation_suffix: String,
    #[serde(default)]
    pub include_stderr_in_reply: bool,
    /// Whether to surface CLI errors as a reply to the user. Spec field —
    /// ilink-hub does not override it at a higher level.
    #[serde(default = "default_true")]
    pub send_error_reply: bool,
    /// Enable partial streaming forwarding (bridge-side hint). Default `true`.
    #[serde(default = "default_true")]
    pub streaming: bool,
    /// Enable the optional tool-permission channel (agentproc 0.4). Default
    /// `false`. `true` keeps stdin open for `permission_request`/`response`.
    #[serde(default)]
    pub permission: bool,
}

/// Resolved bridge profile — the flat, ready-to-run form parsed from
/// [`BridgeProfileFile`]. The `agentproc:` block fields are lifted onto this
/// struct; `script:` (a file-level sibling) is expanded into `command`/`args`
/// at load time. `executor` carries the agentproc in-process executor name
/// directly (replacing the old `type:` shorthand).
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
/// An explicit `agentproc.command` always wins over `script`.
#[derive(Debug, Clone)]
pub struct BridgeProfile {
    /// agentproc in-process executor name (e.g. `claude-code`, `codex`,
    /// `cursor`, `codebuddy`, `agy`). `None` → spawn `command`/`args`.
    pub executor: Option<String>,

    /// Path to a script file. Expanded into `command`/`args` at load time;
    /// an explicit `command` takes priority. Kept for diagnostics after expand.
    pub script: Option<String>,

    pub command: String,
    pub args: Vec<String>,
    /// Working directory for the agent process. Relative paths resolve against
    /// `{{PROFILE_DIR}}` (the profile file's directory); omitted defaults to the
    /// bridge's process cwd. `~` and placeholders are expanded.
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    /// Restrict which `${VAR}` references the `env` block may expand against the
    /// bridge's environment. Absent = expand against the full environment
    /// (profiles are trusted input). Present = a `${VAR}` not in the list
    /// expands to empty with a stderr warning. Names must match exactly.
    pub env_allowlist: Option<Vec<String>>,
    /// CLI 主操作超时（秒），只覆盖 stdout 读取阶段。子进程退出后还有额外的
    /// 10s `child.wait()` 等待，因此最坏情况总耗时为 `timeout_secs + 10s`。
    /// 默认 1800s（30 分钟）。
    pub timeout_secs: u64,
    /// SIGTERM → SIGKILL 宽限期（秒），默认 5。
    pub kill_grace_secs: u64,
    pub max_reply_chars: usize,
    pub truncation_suffix: String,
    pub include_stderr_in_reply: bool,
    /// 是否启用流式 partial 转发（bridge 侧 hint，不进 wire）。默认 `true`。
    /// 设为 `false` 时，agent 仍会输出 `{"type":"partial"}` 事件，但 bridge
    /// 不转发，只从 `{"type":"result"}` 事件取最终回复一次性发送。
    pub streaming: bool,
    /// 启用可选的 tool-permission 通道（agentproc 0.4）。默认 `false`，bridge
    /// 写完 turn 对象即关闭 stdin；`true` 时保持 stdin 开着，处理
    /// `{"type":"permission_request"}` 并写回 `{"type":"permission_response"}`。
    /// ilink-hub 对 permission_request 恒 allow（不再有 per-profile 策略）。
    pub permission: bool,
    /// 是否把 CLI 失败回复给用户。agentproc 规范字段，ilink-hub 不在更高层覆盖。
    pub send_error_reply: bool,
    /// Agent 描述（用于 Hub MCP list_agents 工具返回，让其他 Agent 了解此 Agent 的能力）。
    pub description: Option<String>,
}

impl Default for BridgeProfile {
    fn default() -> Self {
        Self {
            executor: None,
            script: None,
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            env_allowlist: None,
            timeout_secs: default_timeout_secs(),
            kill_grace_secs: default_kill_grace_secs(),
            max_reply_chars: default_max_reply_chars(),
            truncation_suffix: default_truncation_suffix(),
            include_stderr_in_reply: false,
            streaming: true,
            permission: false,
            send_error_reply: true,
            description: None,
        }
    }
}

fn default_timeout_secs() -> u64 {
    1800
}

fn default_kill_grace_secs() -> u64 {
    5
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

/// Loaded bridge configuration: exactly one resolved profile (one file == one
/// profile). No routing, no multi-profile map — the bridge child registers
/// with the Hub under a single name derived from the file.
#[derive(Debug, Clone)]
pub struct BridgeApp {
    name: String,
    profile: BridgeProfile,
    transport: TransportKind,
    via: Via,
    direct_base_url: Option<String>,
}

impl BridgeApp {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("default")
            .to_string();
        Self::parse_yaml(&raw, stem).with_context(|| format!("parse YAML {}", path.display()))
    }

    /// Parse YAML from a string (same as file body). `name` is the profile
    /// name (usually the file stem); used in errors and as the registered name.
    pub fn parse_yaml(raw: &str, name: String) -> Result<Self> {
        let file: BridgeProfileFile =
            serde_norway::from_str(raw).context("serde_norway::from_str BridgeProfileFile")?;
        Self::from_file(file, name)
    }

    fn from_file(file: BridgeProfileFile, name: String) -> Result<Self> {
        let block = file.agentproc;
        let mut profile = BridgeProfile {
            executor: block.executor,
            script: file.script,
            command: block.command,
            args: block.args,
            cwd: block.cwd,
            env: block.env,
            env_allowlist: block.env_allowlist,
            timeout_secs: block.timeout_secs,
            kill_grace_secs: block.kill_grace_secs,
            max_reply_chars: block.max_reply_chars,
            truncation_suffix: block.truncation_suffix,
            include_stderr_in_reply: block.include_stderr_in_reply,
            streaming: block.streaming,
            permission: block.permission,
            send_error_reply: block.send_error_reply,
            description: file.description,
        };

        // Expand the `script:` sibling into command/args when no explicit
        // command was set. An explicit `agentproc.command` always wins.
        profile = expand_script_field(profile, &name)?;

        // An in-process executor with an empty command is valid (the runner
        // drives the CLI directly). Without an executor, command is required.
        let has_executor = profile
            .executor
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty());
        if !has_executor && profile.command.trim().is_empty() {
            anyhow::bail!(
                "profile `{name}`: `agentproc.command` must not be empty \
                 (set `executor`, `command`, or `script`)"
            );
        }
        reject_shell_injection_risk(&profile, &name)?;

        Ok(Self {
            name,
            profile,
            transport: file.transport,
            via: file.via,
            direct_base_url: file.base_url,
        })
    }

    /// Pick profile and payload text for CLI. Single-profile: the text is
    /// passed through unchanged (no prefix routing).
    pub fn resolve<'a>(&'a self, text: &str) -> Result<(&'a str, &'a BridgeProfile, String)> {
        Ok((self.name.as_str(), &self.profile, text.to_string()))
    }

    pub fn profile_names(&self) -> Vec<&str> {
        vec![self.name.as_str()]
    }

    pub fn profile(&self, _name: &str) -> Option<&BridgeProfile> {
        Some(&self.profile)
    }

    pub fn default_profile_name(&self) -> &str {
        &self.name
    }

    pub fn routing_label(&self) -> &'static str {
        "fixed"
    }

    /// Whether CLI failures are surfaced to the user (spec `send_error_reply`).
    pub fn send_error_reply(&self) -> bool {
        self.profile.send_error_reply
    }

    /// Configured IM protocol (`transport:`). Default `ilink`.
    pub fn transport(&self) -> &TransportKind {
        &self.transport
    }

    /// Configured credential/connection mode (`via:`). Default `hub`.
    pub fn via(&self) -> Via {
        self.via
    }

    /// Optional `base_url:` override for `via: direct` — points the transport at
    /// a specific real iLink upstream, overriding `--hub-url` / `WEIXIN_BASE_URL`
    /// for this profile. `None` when unset (fall back to the CLI/env URL).
    pub fn direct_base_url(&self) -> Option<&str> {
        self.direct_base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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

/// Reject profiles with a dangerous shell-injection pattern:
/// a shell interpreter (bash/sh/zsh/fish/dash) as the command with `-c` as an
/// arg AND `{{MESSAGE}}` somewhere in the args — user input would be
/// interpolated into a shell command string, enabling arbitrary command
/// execution.
///
/// `tokio::process::Command` does NOT invoke a shell automatically, so this is
/// only dangerous when the user explicitly invokes a shell with `-c`.
/// Safe alternatives: pass the message via the stdin turn object (always done
/// under agentproc 0.3), or use a non-shell command.
///
/// Only the dangerous combo is rejected; shell + `-c` without `{{MESSAGE}}`,
/// or shell with no placeholder in args/env, still loads.
fn reject_shell_injection_risk(p: &BridgeProfile, name: &str) -> Result<()> {
    // Include common POSIX / busybox shells. Interpreters (python -c, etc.)
    // and wrapper cmds (env/nice) are out of this round's scope.
    const SHELL_CMDS: &[&str] = &[
        "bash", "sh", "zsh", "fish", "dash", "ksh", "mksh", "ash", "busybox",
    ];
    let cmd = std::path::Path::new(&p.command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p.command);
    if !SHELL_CMDS.contains(&cmd) {
        return Ok(());
    }
    let has_dash_c = p.args.iter().any(|a| arg_enables_shell_command_string(a));
    // MESSAGE in args OR env values is the same RCE class when paired with shell -c
    // (e.g. `bash -c "$MSG"` with `env: {MSG: "{{MESSAGE}}"}`).
    let has_message_placeholder = p.args.iter().any(|a| a.contains("{{MESSAGE}}"))
        || p.env.values().any(|v| v.contains("{{MESSAGE}}"));
    if has_dash_c && has_message_placeholder {
        anyhow::bail!(
            "profile `{name}`: SECURITY: shell command with `-c` and `{{{{MESSAGE}}}}` in args \
             or env is rejected — user input would be interpolated into a shell command string. \
             Use `stdin: message` to pass the message safely via stdin instead."
        );
    }
    Ok(())
}

/// True when `arg` enables shell's "run command string" mode (`-c`).
///
/// Matches exact `-c` and combined short options that include `c` after a
/// single leading `-` (e.g. `-lc`, `-ic`, `-xc`, `-cl`). Does **not** match
/// long options like `--color` (double-dash).
fn arg_enables_shell_command_string(arg: &str) -> bool {
    if arg == "-c" {
        return true;
    }
    // Single-dash short cluster only: `-` + one or more flag letters.
    if arg.starts_with('-') && !arg.starts_with("--") {
        return arg
            .as_bytes()
            .get(1..)
            .is_some_and(|flags| flags.contains(&b'c'));
    }
    false
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

/// AgentProc 0.4 `${VAR}` expansion with `env_allowlist` filtering and POSIX
/// "unknown variable → empty string" semantics.
///
/// Differs from [`expand_env_var_named`] in two ways, both required by the 0.3
/// spec:
/// - When `allowlist` is `Some`, a `${VAR}` whose name is **not** in the list
///   expands to the empty string and a WARN is logged (the process still
///   starts — a typo surfaces as an empty variable, not a hard failure).
/// - Unknown variables (not present in `env`) expand to the empty string
///   rather than erroring, matching POSIX shell behaviour. A missing secret
///   therefore surfaces downstream as an auth error from the CLI, not here.
///
/// `$$` still collapses to a literal `$`; invalid identifiers (`${}`, `${1FOO}`)
/// remain hard errors because they signal a malformed profile, not a missing
/// environment value.
#[allow(dead_code)] // MIGRATION: only used by executor.rs run_cli (dead); remove in cleanup task
pub fn expand_env_var_named_with_allowlist(
    template: &str,
    env: &std::collections::HashMap<String, String>,
    allowlist: Option<&[String]>,
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
        if i + 1 >= len {
            out.push('$');
            i += 1;
            continue;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'{' => {
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
                validate_env_ident(ident, template, profile, field)?;
                if let Some(list) = allowlist {
                    if !list.iter().any(|name| name == ident) {
                        tracing::warn!(
                            profile = profile.unwrap_or(""),
                            field = field.unwrap_or(""),
                            var = ident,
                            "env_allowlist blocked ${{{}}}; expanded to empty",
                            ident
                        );
                        i = end + 1;
                        continue;
                    }
                }
                let value = env.get(ident).map(|s| s.as_str()).unwrap_or("");
                out.push_str(value);
                i = end + 1;
            }
            _ => {
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

    fn parse(y: &str) -> Result<BridgeApp> {
        BridgeApp::parse_yaml(y, "bot".to_string())
    }

    #[test]
    fn transport_and_via_default_to_ilink_hub() {
        // No `transport:` / `via:` → stage-2 defaults: ilink via hub (no regression).
        let y = r#"
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert!(app.transport().is_ilink());
        assert_eq!(app.transport().as_str(), "ilink");
        assert!(app.via().is_hub());
    }

    #[test]
    fn transport_other_loads_as_placeholder_name() {
        let y = r#"
transport: telegram
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert!(!app.transport().is_ilink());
        assert_eq!(app.transport().as_str(), "telegram");
    }

    #[test]
    fn via_direct_parses() {
        let y = r#"
via: direct
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert!(app.via().is_direct());
    }

    #[test]
    fn base_url_override_parses_for_direct() {
        let y = r#"
via: direct
base_url: https://ilinkai.weixin.qq.com
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert_eq!(app.direct_base_url(), Some("https://ilinkai.weixin.qq.com"));
    }

    #[test]
    fn base_url_default_is_none() {
        let y = r#"
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert!(app.direct_base_url().is_none());
    }

    #[test]
    fn via_unknown_is_rejected() {
        let y = r#"
via: sideways
agentproc:
  command: echo
"#;
        let err = parse(y).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sideways"),
            "expected unknown via in error: {msg}"
        );
        assert!(
            msg.contains("via"),
            "expected `via` context in error: {msg}"
        );
    }

    #[test]
    fn parse_agentproc_block_yaml() {
        let y = r#"
agentproc:
  command: echo
  args: ["{{MESSAGE}}"]
"#;
        let app = parse(y).unwrap();
        assert_eq!(app.profile_names(), vec!["bot"]);
        let (n, p, payload) = app.resolve("hello").unwrap();
        assert_eq!(n, "bot");
        assert_eq!(p.command, "echo");
        assert_eq!(payload, "hello");
    }

    #[test]
    fn script_field_py_expands_to_python3() {
        let y = r#"
script: ./my_handler.py
agentproc:
  timeout_secs: 60
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hello").unwrap();
        assert_eq!(p.command, "python3");
        assert_eq!(p.args, vec!["./my_handler.py"]);
    }

    #[test]
    fn script_field_js_expands_to_node() {
        let y = r#"
script: ./handler.js
agentproc: {}
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "node");
        assert_eq!(p.args, vec!["./handler.js"]);
    }

    #[test]
    fn script_field_ts_expands_to_npx_tsx() {
        let y = r#"
script: ./handler.ts
agentproc: {}
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "npx");
        assert_eq!(p.args, vec!["tsx", "./handler.ts"]);
    }

    #[test]
    fn script_field_sh_expands_to_bash() {
        let y = r#"
script: ./run.sh
agentproc: {}
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "bash");
        assert_eq!(p.args, vec!["./run.sh"]);
    }

    #[test]
    fn explicit_command_wins_over_script() {
        let y = r#"
script: ./handler.py
agentproc:
  command: /usr/bin/python3.11
  args: ["./handler.py"]
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "/usr/bin/python3.11");
    }

    #[test]
    fn executor_without_command_is_valid() {
        // An in-process executor drives the CLI directly; `command` may be empty.
        let y = r#"
agentproc:
  executor: claude-code
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.executor.as_deref(), Some("claude-code"));
        assert_eq!(p.command, "");
    }

    #[test]
    fn no_executor_and_no_command_errors() {
        let y = r#"
agentproc: {}
"#;
        let err = parse(y).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("command"), "{msg}");
    }

    #[test]
    fn recursive_profile_uses_builtin_spawn_form() {
        // recursive has no in-process executor; it spawns the bridge builtin.
        let y = r#"
agentproc:
  command: ilink-hub-bridge
  args: [profile, recursive]
  cwd: ~/projects/recursive
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "ilink-hub-bridge");
        assert_eq!(p.args, vec!["profile", "recursive"]);
    }

    #[test]
    fn send_error_reply_defaults_true_and_passes_through() {
        let y = r#"
agentproc:
  command: echo
"#;
        let app = parse(y).unwrap();
        assert!(app.send_error_reply());

        let y2 = r#"
agentproc:
  command: echo
  send_error_reply: false
"#;
        let app2 = parse(y2).unwrap();
        assert!(!app2.send_error_reply());
    }

    #[test]
    fn shell_c_with_message_placeholder_rejected() {
        let y = r#"
agentproc:
  command: bash
  args: ["-c", "echo {{MESSAGE}}"]
"#;
        let err = parse(y).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SECURITY") && msg.contains("MESSAGE"),
            "expected shell-injection reject, got: {msg}"
        );
    }

    #[test]
    fn shell_without_dash_c_still_loads() {
        // A shell running a script (no `-c`) is fine; the message travels via
        // the stdin turn object, never via argv.
        let y = r#"
agentproc:
  command: bash
  args: ["./run.sh"]
"#;
        let app = parse(y).unwrap();
        let (_, p, _) = app.resolve("hi").unwrap();
        assert_eq!(p.command, "bash");
    }

    #[test]
    fn shell_c_without_message_placeholder_still_loads() {
        // Only the dangerous combo (shell + -c + {{MESSAGE}}) is rejected.
        let y = r#"
agentproc:
  command: bash
  args: ["-c", "echo hello"]
"#;
        assert!(parse(y).is_ok());
    }

    #[test]
    fn shell_lc_with_message_placeholder_rejected() {
        // Combined short options (-lc / -ic / -xc) must count as -c.
        let y = r#"
agentproc:
  command: bash
  args: ["-lc", "echo {{MESSAGE}}"]
"#;
        let err = parse(y).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SECURITY") && msg.contains("MESSAGE"),
            "expected -lc shell-injection reject, got: {msg}"
        );
    }

    #[test]
    fn shell_c_with_message_in_env_rejected() {
        // MESSAGE via env + bash -c $MSG is the same RCE class as args.
        let y = r#"
agentproc:
  command: bash
  args: ["-c", "$MSG"]
  env:
    MSG: "{{MESSAGE}}"
"#;
        let err = parse(y).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SECURITY") && msg.contains("MESSAGE"),
            "expected env-based shell-injection reject, got: {msg}"
        );
    }

    #[test]
    fn shell_long_option_color_not_treated_as_dash_c() {
        // `--color` must not false-positive as enabling -c.
        let y = r#"
agentproc:
  command: bash
  args: ["--color", "echo {{MESSAGE}}"]
"#;
        assert!(
            parse(y).is_ok(),
            "long option --color must not trigger -c reject"
        );
    }

    #[test]
    fn ksh_c_with_message_placeholder_rejected() {
        let y = r#"
agentproc:
  command: ksh
  args: ["-c", "echo {{MESSAGE}}"]
"#;
        let err = parse(y).unwrap_err();
        assert!(
            err.to_string().contains("SECURITY"),
            "ksh -c + MESSAGE must be rejected"
        );
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
