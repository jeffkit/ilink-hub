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

    cmd.env("ILINK_MESSAGE", message);
    cmd.env("ILINK_SESSION_ID", "");
    cmd.env("ILINK_SESSION_NAME", "default");
    cmd.env("ILINK_FROM_USER", "probe");
    cmd.env("ILINK_CONTEXT_TOKEN", "probe");

    for (k, v) in &profile.env {
        let v = v
            .replace("{{MESSAGE}}", message)
            .replace("{{SESSION_ID}}", "")
            .replace("{{SESSION_NAME}}", "default");
        cmd.env(k, v);
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(ProbeError::NotFound(resolved_command));
            }
            return Err(ProbeError::ExecutionError(format!("无法启动进程: {e}")));
        }
    };

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
