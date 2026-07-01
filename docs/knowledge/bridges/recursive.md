---
type: Reference
title: Recursive Bridge
description: 内置 recursive Bridge 的配置、环境变量与 session 持久化说明。
tags: [bridge, recursive, profile]
timestamp: 2026-06-30T09:00:00+08:00
---

# Recursive Bridge

内置 `recursive` profile 类型，包装 [`recursive`](https://github.com/jeffkit/recursive) agent CLI，支持 session 持久化与 resume。

## 前置条件

`recursive` CLI 需在 PATH 中可用，或通过 `RECURSIVE_BIN` 指定完整路径。

```bash
# 通过 brew 安装（推荐）
brew install jeffkit/tap/recursive

# 验证版本
/opt/homebrew/bin/recursive --version
# recursive 0.7.0
```

## Profile YAML 示例

```yaml
# ~/ilink-hub-bridge/profiles/recursive.yaml
profiles:
  recursive:
    type: recursive
    cwd: ~/projects/recursive
    env:
      RECURSIVE_BIN: /opt/homebrew/bin/recursive   # 显式指定 brew 路径
      RECURSIVE_WORKSPACE: ~/projects/recursive
      RECURSIVE_MODEL: deepseek-v4-flash
      RECURSIVE_PROVIDER: anthropic
      RECURSIVE_API_BASE: https://api.deepseek.com/anthropic
      RECURSIVE_API_KEY: ${DEEPSEEK_API_KEY}        # 从 launchd plist 环境变量展开

routing:
  strategy: fixed
  default_profile: recursive
```

> `type: recursive` 是内置简写，展开为 `ilink-hub-bridge profile recursive`。
> 必须使用 `profiles:` + `routing:` 的多 profile 格式；legacy flat 格式（无 `profiles:` 顶层 key）不会识别 `type:` 字段。
> `${DEEPSEEK_API_KEY}` 从 bridge-manager 进程环境变量展开（通常写在 launchd plist 的 `EnvironmentVariables` 里），与 `ilink-hub-glm.yaml` 用 `${GLM_API_KEY}` 的惯例一致。

## 环境变量

| 变量 | 说明 |
|------|------|
| `RECURSIVE_BIN` | binary 完整路径（默认 `recursive`，brew 安装推荐设为 `/opt/homebrew/bin/recursive`） |
| `RECURSIVE_WORKSPACE` | agent 可读写的工作区根目录（**推荐设置**） |
| `RECURSIVE_MODEL` | 覆盖模型（如 `deepseek-chat`、`claude-sonnet-4-5`） |
| `RECURSIVE_PROVIDER` | 覆盖 provider（`openai` 或 `anthropic`） |
| `RECURSIVE_API_KEY` | 覆盖 API Key |
| `RECURSIVE_API_BASE` | 覆盖 API base URL |
| `RECURSIVE_MAX_STEPS` | 最大 agent 循环次数（默认取 `~/.recursive/config`） |

标准 P0 变量（`AGENT_MESSAGE`、`AGENT_SESSION_ID` 等）由 Hub 自动注入。

## Session 持久化原理

`recursive` 在运行时将 session 写入：

```
~/.recursive/workspaces/<hash>/sessions/<slug>/<uuid>/
```

Bridge 从 stderr 中捕获以下格式的行来提取 session UUID：

```
session: recording to /…/sessions/<slug>/<uuid>/
session: appending to /…/sessions/<slug>/<uuid>/
session: saved N message(s) to /…/sessions/<slug>/<uuid>/
```

提取出的 UUID 作为 Hub session ID 持久化，下次用户发消息时通过 `-r <uuid>` 恢复对话。

## 底层调用格式

```bash
# 新会话
recursive --headless --output-format stream-json -p "<message>"

# 恢复会话
recursive --headless --output-format stream-json -r <session-uuid> -p "<message>"
```

## 输出事件

Bridge 只处理 `assistant_text` 类型的 JSON 事件：

```json
{"type":"assistant_text","text":"...","step":1}
```

每个非空 `assistant_text` 事件立即以 `AGENT_PARTIAL:` 行发送给微信用户。
