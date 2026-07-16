---
type: Bridge
title: Claude Code Bridge
description: 内置 Bridge，包装 Anthropic Claude Code CLI，支持会话续传、流式输出与多模态附件（AgentProc 0.4）。
resource: src/bridge/builtin/claude_code.rs
tags: [bridge, claude, builtin, streaming, agentproc]
timestamp: 2026-07-16T14:30:00+08:00
---

# Claude Code Bridge

内置 Bridge，包装 **`claude` CLI**（Anthropic Claude Code），实现会话连续性、流式输出与多模态附件。采用 AgentProc 0.4 NDJSON 协议。

## Profile 示例

```yaml
description: Claude Code on project
agentproc:
  executor: claude-code
  cwd: /path/to/project
  streaming: true
  # env:
  #   CLAUDE_MODEL: sonnet
  #   ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

关闭流式：

```yaml
description: Claude oneshot
agentproc:
  executor: claude-code
  cwd: /path/to/project
  streaming: false
```

## 工作原理

1. 从 stdin 读取 [AgentProc 0.4 turn](profile-protocol.md)
2. 调用 `claude --output-format stream-json [--resume <uuid>]`
3. 消息通过 **`-p` 参数**传入（多模态走 `--input-format stream-json`）
4. `assistant` 文本块 → `partial`；终端 `result` → AgentProc `result`（带 `session_id`）

`streaming` 是 bridge 侧 hint：agent 始终以 stream-json 运行；`streaming: false` 时 Bridge 不转发 `partial`。

## 会话续传

- turn 的 `session_id` 非空时先尝试 `--resume <uuid>`
- Resume 失败时自动回退新会话

## 多模态附件

turn 的 `attachments` 支持 `image` / `file`：

- `image`：base64 → Anthropic `image` content block
- `file`：PDF / 纯文本 → `document` content block
- `video` 等：`error` 事件拒绝

## Permission 模式

`permission: true` 时切换到 `--permission-prompt-tool stdio --permission-mode default`，转译 Claude `control_request` ↔ AgentProc `permission_request`。**当前 Bridge 对 permission_request 恒 allow**（已移除 WeChat ask / `permission_default`）。

```yaml
description: Claude with permission channel
agentproc:
  executor: claude-code
  cwd: /path/to/project
  permission: true
```

## 已知差异（vs Cursor Bridge）

| 特性 | Claude Code | [Cursor](cursor.md) |
|------|-------------|---------------------|
| 消息传入 | `-p`；多模态 stream-json | stdin 原始文本 |
| CLI | `claude` | `agent` |
| 多模态 | image / file | 不支持 |

## 相关文档

- [AgentProc 0.4 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
