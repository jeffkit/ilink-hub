---
type: Bridge
title: Claude Code Bridge
description: 内置 Bridge，包装 Anthropic Claude Code CLI，支持会话续传、流式和一次性输出两种模式。
resource: src/bridge/builtin/claude_code.rs
tags: [bridge, claude, builtin, streaming]
timestamp: 2026-06-18T10:00:00+08:00
---

# Claude Code Bridge

内置 Bridge，包装 **`claude` CLI**（Anthropic Claude Code），实现会话连续性和流式输出。

## 工作原理

1. 读取 [P0 环境变量](/bridges/profile-protocol.md)（`AGENT_MESSAGE`、`AGENT_SESSION_ID`、`AGENT_STREAMING` 等）
2. 调用 `claude --output-format stream-json [--resume <uuid>]`
3. 消息通过 **`-p` 参数**传入（与 Cursor 不同，后者用 stdin）
4. 实时解析 stream-json 事件，每个 assistant 文本块输出 `AGENT_PARTIAL:<json>`
5. 流结束后输出 `AGENT_SESSION:<new_session_id>`，不再输出正文

## 流式 vs 一次性模式

| `AGENT_STREAMING` | 行为 |
|-------------------|------|
| `1`（默认） | 实时发送 `AGENT_PARTIAL:` 分块，用户边生成边看到 |
| `0` | 等 AI 完全响应后一次性写入 stdout，调试时用 |

关闭流式（Profile YAML）：

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/project
    streaming: false
```

## 会话续传

- 若 `AGENT_SESSION_ID` 非空，先尝试 `--resume <uuid>`
- Resume 失败时自动回退新会话，用户透明

## stream-json 事件格式

```json
// type == "assistant" 事件
{ "type": "assistant", "message": { "content": [{ "type": "text", "text": "..." }] } }

// type == "result" 事件
{ "type": "result", "session_id": "<uuid>", "result": "...", "subtype": "success" }
```

## 已知差异（vs Cursor Bridge）

| 特性 | Claude Code Bridge | [Cursor Bridge](cursor.md) |
|------|-------------------|---------------------------|
| 消息传入方式 | `-p` 参数 | stdin |
| CLI 工具 | `claude` | `agent` |
| `AGENT_STREAMING=0` 支持 | 是 | 否 |

## 相关文档

- [P0 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
