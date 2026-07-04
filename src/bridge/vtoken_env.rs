//! Bridge vtoken 环境变量辅助功能。
//!
//! Bridge 启动时自动注册到 Hub 并获得 vtoken，本模块提供从凭证文件读取 vtoken 的功能，
//! 供 Agent 子进程配置 MCP 连接使用。

use anyhow::{Context, Result};

/// 从 Bridge 凭证文件读取 vtoken。
///
/// 凭证文件路径默认为 `~/.ilink-hub/bridge-credentials.json`，可通过环境变量
/// `ILINKHUB_BRIDGE_CREDS` 自定义。
///
/// # 返回值
///
/// 返回 `(hub_url, vtoken)` 元组。
///
/// # 错误
///
/// - 文件不存在
/// - JSON 解析失败
/// - token 字段为空
pub fn read_bridge_credentials(path: Option<&str>) -> Result<(String, String)> {
    let cred_path = path
        .map(PathBuf::from)
        .unwrap_or_else(super::default_local_credential_path);

    let data = std::fs::read_to_string(&cred_path)
        .with_context(|| format!("read {}", cred_path.display()))?;

    let creds: serde_json::Value =
        serde_json::from_str(&data).with_context(|| format!("parse {}", cred_path.display()))?;

    let token = creds["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("token field missing or empty in credentials")?;

    let base_url = creds["base_url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("base_url field missing or empty in credentials")?;

    Ok((base_url.to_string(), token.to_string()))
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires actual credentials file"]
    fn test_read_credentials() {
        let (url, token) = read_bridge_credentials(None).unwrap();
        assert!(!url.is_empty());
        assert!(!token.is_empty());
    }
}
