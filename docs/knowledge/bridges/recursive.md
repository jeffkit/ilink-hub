---
type: Reference
title: Recursive Bridge
description: 内置 recursive Bridge 的配置、环境变量与 session 持久化说明（AgentProc 0.4）。
tags: [bridge, recursive, profile, agentproc]
timestamp: 2026-07-16T14:30:00+08:00
---

# Recursive Bridge

内置 `recursive` executor，包装 [`recursive`](https://github.com/jeffkit/recursive) agent CLI，支持 session 持久化与 resume。采用 AgentProc 0.4 NDJSON 协议。

## 前置条件

`recursive` **≥ 0.8.0** 需在 PATH 中可用，或通过 `RECURSIVE_BIN` 指定完整路径。

```bash
cd ~/projects/recursive && git checkout v0.8.0
cargo install --path crates/recursive-cli --force
~/.cargo/bin/recursive --version
```

## Profile YAML 示例

```yaml
# ~/.ilink-hub-bridge/profiles/recursive.yaml
description: Recursive agent
agentproc:
  executor: recursive
  cwd: ~/projects/recursive
  env:
    RECURSIVE_BIN: ~/.cargo/bin/recursive
    RECURSIVE_WORKSPACE: ~/projects/recursive
    RECURSIVE_MODEL: deepseek-v4-flash
    RECURSIVE_PROVIDER: anthropic
    RECURSIVE_API_BASE: https://api.deepseek.com/anthropic
    RECURSIVE_API_KEY: ${DEEPSEEK_API_KEY}
```

> `${DEEPSEEK_API_KEY}` 从 bridge-manager 进程环境展开（通常写在 launchd plist `EnvironmentVariables`）。

## 环境变量

| 变量 | 说明 |
|------|------|
| `RECURSIVE_BIN` | binary 路径（需 0.8+） |
| `RECURSIVE_WORKSPACE` | 工作区根目录（推荐） |
| `RECURSIVE_MODEL` | 模型名 |
| `RECURSIVE_PROVIDER` | `openai` / `anthropic` |
| `RECURSIVE_API_KEY` / `RECURSIVE_API_BASE` | API 凭据 |
| `RECURSIVE_MAX_STEPS` | 最大循环次数 |

## Session 持久化

`recursive` 将 session 写到 `~/.recursive/workspaces/<hash>/sessions/...`。Bridge 从 stderr 捕获 `session: recording/appending/saved …/<uuid>/` 行，把 UUID 作为 Hub session ID，下次用 `-r <uuid>` 恢复。

## 底层调用

```bash
recursive --headless --output-format stream-json -p "<message>"
recursive --headless --output-format stream-json -r <session-uuid> -p "<message>"
```

CLI stream-json 事件由 builtin 转译为 AgentProc 0.4 的 `partial` / `result` / `error`。

## 相关文档

- [AgentProc 0.4 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
