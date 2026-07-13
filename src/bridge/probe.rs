use anyhow::Result;

use crate::bridge::config::BridgeProfile;
use crate::bridge::paths::find_in_path;
use crate::bridge::AUTH_ERROR_KEYWORDS;
use crate::paths::expand_user_path;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, thiserror::Error)]
#[serde(rename_all = "camelCase")]
pub enum ProbeError {
    #[error("未找到 `{0}` 命令，请先安装该 CLI 工具并确保其在 PATH 中")]
    NotFound(String),
    #[error("项目目录不存在: {0}")]
    ConfigError(String),
    #[error("未认证，请先登录 CLI: {0}")]
    Unauthenticated(String),
    #[error("CLI 执行失败: {0}")]
    ExecutionError(String),
}

impl ProbeError {
    pub fn error_type(&self) -> &'static str {
        match self {
            ProbeError::NotFound(_) => "NotFound",
            ProbeError::ConfigError(_) => "ConfigError",
            ProbeError::Unauthenticated(_) => "Unauthenticated",
            ProbeError::ExecutionError(_) => "ExecutionError",
        }
    }
}

pub fn find_in_path_robust(name: &str) -> Option<std::path::PathBuf> {
    if let Some(path) = find_in_path(name) {
        return Some(path);
    }
    #[cfg(not(windows))]
    {
        let common_dirs = [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ];
        for &dir in &common_dirs {
            let candidate = std::path::Path::new(dir).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let home_path = std::path::Path::new(&home);
            for rel in &[".local/bin", ".npm-global/bin", ".cargo/bin"] {
                let candidate = home_path.join(rel).join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

pub fn check_command_exists(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains('/') || command.contains('\\') {
        let path = std::path::Path::new(command);
        return path.exists() && path.is_file();
    }
    find_in_path_robust(command).is_some()
}

pub fn probe_profile_light(profile: &BridgeProfile) -> Result<(), ProbeError> {
    if let Some(dir) = &profile.cwd {
        let expanded_dir = expand_user_path(
            &dir.replace("{{MESSAGE}}", "ping")
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        let path = std::path::Path::new(&expanded_dir);
        if !path.exists() {
            return Err(ProbeError::ConfigError(expanded_dir));
        }
    }

    let command_to_check: &str = match profile.profile_type.as_deref() {
        Some("claude-code") => "claude",
        Some("codex") => "codex",
        Some("cursor") => "cursor",
        Some("agy") => "agy",
        _ => match profile.command.as_str() {
            "claude" => "claude",
            "codex" => "codex",
            "cursor" => "cursor",
            "agy" => "agy",
            other => other,
        },
    };

    if !check_command_exists(command_to_check) {
        return Err(ProbeError::NotFound(command_to_check.to_string()));
    }

    Ok(())
}

pub async fn dry_run_profile(profile: &BridgeProfile, message: &str) -> Result<String, ProbeError> {
    if let Some(dir) = &profile.cwd {
        let expanded_dir = expand_user_path(
            &dir.replace("{{MESSAGE}}", message)
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        let path = std::path::Path::new(&expanded_dir);
        if !path.exists() {
            return Err(ProbeError::ConfigError(expanded_dir));
        }
    }

    let command = if profile.command == "ilink-hub-bridge" {
        match profile.profile_type.as_deref() {
            Some("claude-code") => "claude".to_string(),
            Some("codex") => "codex".to_string(),
            Some("cursor") => "cursor".to_string(),
            Some("agy") => "agy".to_string(),
            _ => profile.command.clone(),
        }
    } else {
        profile.command.clone()
    };

    if !check_command_exists(&command) {
        return Err(ProbeError::NotFound(command));
    }

    const KNOWN_AGENT_CLIS: &[&str] = &["claude", "agent", "codex", "cursor", "agy"];
    let resolved_command = if KNOWN_AGENT_CLIS.contains(&command.as_str()) {
        find_in_path_robust(&command)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(command)
    } else {
        command
    };

    let args: Vec<String> = match profile.profile_type.as_deref() {
        Some("claude-code") => vec![
            "--output-format".into(),
            "json".into(),
            "--dangerously-skip-permissions".into(),
            "--disallowed-tools".into(),
            "AskUserQuestion".into(),
            "-p".into(),
            message.to_string(),
        ],
        Some("codex") => vec![
            "--approval-mode".into(),
            "full-auto".into(),
            "-q".into(),
            message.to_string(),
        ],
        Some("cursor") => vec![
            "agent".into(),
            "run".into(),
            "--prompt".into(),
            message.to_string(),
        ],
        Some("agy") => vec![
            "--dangerously-skip-permissions".into(),
            "-p".into(),
            message.to_string(),
        ],
        _ => profile
            .args
            .iter()
            .map(|a| {
                a.replace("{{MESSAGE}}", message)
                    .replace("{{SESSION_ID}}", "")
                    .replace("{{SESSION_NAME}}", "default")
            })
            .collect(),
    };

    let mut cmd = tokio::process::Command::new(&resolved_command);
    cmd.args(&args);
    cmd.kill_on_drop(true);

    if let Some(dir) = &profile.cwd {
        let expanded_dir = expand_user_path(
            &dir.replace("{{MESSAGE}}", message)
                .replace("{{SESSION_ID}}", "")
                .replace("{{SESSION_NAME}}", "default"),
        );
        cmd.current_dir(&expanded_dir);
    }

    cmd.env("AGENT_CONTEXT_TOKEN", "probe");

    for (k, v) in &profile.env {
        let v = v
            .replace("{{MESSAGE}}", message)
            .replace("{{SESSION_ID}}", "")
            .replace("{{SESSION_NAME}}", "default");
        cmd.env(k, v);
    }

    // Write the agentproc 0.3 turn object to stdin (one NDJSON line, then EOF).
    let turn = crate::bridge::protocol::TurnObject::new(
        message,
        "",
        "default",
        "probe",
        Vec::new(),
        false,
    );
    let turn_line = turn
        .to_ndjson()
        .map_err(|e| ProbeError::ExecutionError(format!("serialize turn object: {e}")))?;
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(ProbeError::NotFound(resolved_command));
            }
            return Err(ProbeError::ExecutionError(format!("无法启动进程: {e}")));
        }
    };

    // Write the turn line and close stdin so the agent finalises its turn.
    if let Some(stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let mut stdin = stdin;
        let _ = stdin.write_all(turn_line.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.shutdown().await;
    }

    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
            .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(ProbeError::ExecutionError(format!("等待进程退出失败: {e}"))),
            Err(_) => return Err(ProbeError::ExecutionError("执行超时 (10s)".to_string())),
        };

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let all_output = format!("{}\n{}", stdout_str, stderr_str).to_lowercase();
        if AUTH_ERROR_KEYWORDS.iter().any(|&k| all_output.contains(k)) {
            return Err(ProbeError::Unauthenticated(format!(
                "exit code: {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
                output.status.code(),
                stderr_str,
                stdout_str
            )));
        }
        return Err(ProbeError::ExecutionError(format!(
            "exit code: {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
            output.status.code(),
            stderr_str,
            stdout_str
        )));
    }

    Ok(stdout_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::config::BridgeProfile;

    // ── ProbeError::error_type ────────────────────────────────────────────────

    /// M9-probe-1: error_type 必须为每个变体返回其对应的静态字符串。
    /// 捕捉 match 分支被替换为 "" 或 "xyzzy" 的变异。
    #[test]
    fn probe_error_type_returns_correct_string_for_all_variants() {
        assert_eq!(ProbeError::NotFound("cmd".into()).error_type(), "NotFound");
        assert_eq!(
            ProbeError::ConfigError("dir".into()).error_type(),
            "ConfigError"
        );
        assert_eq!(
            ProbeError::Unauthenticated("hint".into()).error_type(),
            "Unauthenticated"
        );
        assert_eq!(
            ProbeError::ExecutionError("msg".into()).error_type(),
            "ExecutionError"
        );
    }

    // ── check_command_exists ──────────────────────────────────────────────────

    /// M9-probe-2: 空命令名必须直接返回 false。
    /// 捕捉 `if command.is_empty() { return false }` → `return true` 的变异。
    #[test]
    fn check_command_exists_empty_string_returns_false() {
        assert!(!check_command_exists(""), "empty command must return false");
    }

    /// M9-probe-3: 含路径分隔符的参数走文件存在性检查；不存在的绝对路径必须返回 false。
    /// 捕捉 `command.contains('/')` → `!command.contains('/')` 的变异，
    /// 以及 `path.exists() && path.is_file()` → `path.exists() || path.is_file()` 的变异。
    #[test]
    fn check_command_exists_absolute_nonexistent_path_returns_false() {
        assert!(
            !check_command_exists("/nonexistent-path-ilink-test/cmd"),
            "nonexistent absolute path must return false"
        );
    }

    /// M9-probe-4: 含路径分隔符的参数走文件存在性检查；已知存在的可执行文件必须返回 true。
    /// 捕捉 `path.exists() && path.is_file()` 整体被替换为 `true` 或 `false` 的变异。
    #[cfg(unix)]
    #[test]
    fn check_command_exists_absolute_path_to_real_executable_returns_true() {
        // /bin/sh 在所有 POSIX 系统都存在
        assert!(
            check_command_exists("/bin/sh"),
            "/bin/sh must exist as a file"
        );
    }

    /// M9-probe-5: 不含路径分隔符的命令名走 find_in_path_robust；
    /// 系统中肯定存在 `sh`，必须返回 true。
    #[test]
    fn check_command_exists_command_name_in_path_returns_true() {
        // `sh` 必然在 PATH 中
        assert!(
            check_command_exists("sh"),
            "sh must be found in PATH on any POSIX system"
        );
    }

    /// M9-probe-6: 不在 PATH 中的随机名字必须返回 false。
    /// 捕捉 `find_in_path_robust(command).is_some()` → `true` 的变异。
    #[test]
    fn check_command_exists_unknown_command_returns_false() {
        assert!(
            !check_command_exists("nonexistent-cmd-ilink-test-99"),
            "random command must not be found"
        );
    }

    #[tokio::test]
    async fn test_probe_profile_light_and_dry_run() {
        let profile_missing_cwd = BridgeProfile {
            command: "echo".to_string(),
            cwd: Some("/nonexistent-path-for-sure-12345".to_string()),
            ..Default::default()
        };
        let err = probe_profile_light(&profile_missing_cwd).unwrap_err();
        assert!(matches!(err, ProbeError::ConfigError(_)));

        let profile_missing_cmd = BridgeProfile {
            command: "nonexistent-cli-cmd-12345".to_string(),
            ..Default::default()
        };
        let err2 = probe_profile_light(&profile_missing_cmd).unwrap_err();
        assert!(matches!(err2, ProbeError::NotFound(_)));

        let profile_valid = BridgeProfile {
            command: "echo".to_string(),
            args: vec!["{{MESSAGE}}".to_string()],
            ..Default::default()
        };
        probe_profile_light(&profile_valid).unwrap();

        let res = dry_run_profile(&profile_valid, "hello").await.unwrap();
        assert!(res.contains("hello"));

        let profile_claude_mock = BridgeProfile {
            command: "echo".to_string(),
            profile_type: Some("claude-code".to_string()),
            ..Default::default()
        };
        let res_claude = dry_run_profile(&profile_claude_mock, "ping").await.unwrap();
        assert!(res_claude.contains("--dangerously-skip-permissions"));
        assert!(res_claude.contains("-p"));
        assert!(res_claude.contains("ping"));
    }
}
