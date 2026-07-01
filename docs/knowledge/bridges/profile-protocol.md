---
type: Reference
title: P0 协议与 Bridge Profile
description: Bridge 与 profile 进程之间的 P0 协议契约：环境变量输入、stdout 输出、流式格式。
resource: docs/bridge/profile-spec.md
tags: [bridge, profile, protocol, p0]
timestamp: 2026-06-18T10:00:00+08:00
---

# P0 协议与 Bridge Profile

P0 是 Bridge 与 profile 进程之间的**零依赖通信契约**：仅靠环境变量 + stdout，无 SDK，完全跨平台。

## 输入：Bridge 注入的环境变量

| 变量名 | 说明 |
|--------|------|
| `AGENT_MESSAGE` | 用户消息文本（路由后净文本，前缀已剥离） |
| `AGENT_SESSION_ID` | Hub 持久化的后端 session UUID（空 = 新会话） |
| `AGENT_SESSION_NAME` | session 可读名称（默认 `default`） |
| `AGENT_FROM_USER` | 发送消息的用户 ID |
| `AGENT_CONTEXT_TOKEN` | Hub context token |
| `AGENT_STREAMING` | `1`（默认）= 流式，`0` = 一次性 |

> 当 YAML 设置了 `stdin: message` 时，`AGENT_MESSAGE` 同时也写入 stdin。

## 输出：profile 写 stdout

```
[可选] AGENT_SESSION:<uuid>       ← 首行，如需追踪 session
<回复给微信用户的文本>
```

Bridge 从进程启动起**实时读取** stdout，写入即处理。

## 流式输出（AGENT_PARTIAL）

需要分块推送时，在 stdout 中插入：

```
AGENT_PARTIAL:<JSON 编码的字符串>
```

- Bridge 读到该行，**立即**将解码文本发给微信用户
- `<JSON 编码的字符串>` 是 `json.dumps(text)` / `JSON.stringify(text)` 的结果（换行等已转义，整行只占一行）
- 进程退出 = EOF；若 stdout 有剩余非空内容，作为最终消息发送；若全部已由 `AGENT_PARTIAL` 发出，最终 body 为空则跳过

## Profile YAML 结构

### 单 Profile

```yaml
command: python3
args: [/path/to/my_ai.py]
cwd: /path/to/project
```

### 多 Profile（prefix 路由）

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/project
  cursor:
    type: cursor
    cwd: /path/to/project
routing: prefix    # 或 fixed
```

### 关闭流式

```yaml
profiles:
  claude:
    type: claude-code
    streaming: false
```

## 内置 Profile 类型

| `type` 值 | 说明 | 详细文档 |
|-----------|------|---------|
| `claude-code` | 包装 `claude` CLI | [Claude Code Bridge](claude-code.md) |
| `cursor` | 包装 Cursor `agent` CLI | [Cursor Bridge](cursor.md) |
| `codebuddy-code` | 包装 `codebuddy` CLI | — |
| `codex` | 包装 OpenAI `codex` CLI | — |
| `agy` | 包装 Google `agy` CLI | — |
| `recursive` | 包装 `recursive` agent CLI | [Recursive Bridge](recursive.md) |

## 自定义 Profile

任何可执行程序（Python、Node、shell 脚本等）都可以作为 profile，只需：
1. 读 `AGENT_MESSAGE` 环境变量（或 stdin）
2. 把回复写到 stdout

完整示例见 [`examples/`](../../examples/)。
