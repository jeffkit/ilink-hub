---
type: Bridge
title: Cursor Bridge
description: 内置 Bridge，包装 Cursor agent CLI，支持会话续传与 AgentProc 0.3 NDJSON 流式输出。
resource: src/bridge/builtin/cursor.rs
tags: [bridge, cursor, builtin, streaming, agentproc]
timestamp: 2026-07-13T20:30:00+08:00
---

# Cursor Bridge

内置 Bridge，包装 **Cursor `agent` CLI**，实现会话连续性与流式输出。采用 AgentProc 0.3 NDJSON 协议与 Hub 通信。

## 工作原理

1. 从 stdin 读取 [AgentProc 0.3 turn](/bridges/profile-protocol.md)（`message` / `session_id` / `attachments`）
2. 调用 `agent --print --trust --yolo --output-format stream-json [--model <model>] [--resume <uuid>]`
3. 消息写入 `agent` 进程的 **stdin**（与 Claude Code 不同，后者用 `-p` 参数）
4. 实时解析 stream-json 事件：每个 `assistant` 文本块 → `partial` NDJSON 事件；终端 `result.result` → `text` 事件
5. 流结束后输出 `session` 事件上报新 session id

`streaming` 是 bridge 侧 hint：agent 始终以 stream-json 运行；profile `streaming: false` 时 Bridge 不转发 `partial`，仅以 `text` 事件作为最终回复。

## 会话续传

- 若 turn 的 `session_id` 非空，先尝试 `--resume <uuid>`
- 若 resume 失败（session 过期/不存在），自动回退为新会话重试，用户不会看到报错

## stream-json 事件格式

```json
// type == "assistant" 事件（增量文本）
{ "type": "assistant", "message": { "content": [{ "type": "text", "text": "..." }] } }

// type == "result" 事件（最终结果）
{ "type": "result", "session_id": "<uuid>", "result": "..." }
```

## Profile 示例

```yaml
profiles:
  cursor-local:
    type: cursor
    cwd: /path/to/your/project
```

## 已知差异（vs Claude Code Bridge）

| 特性 | Cursor Bridge | [Claude Code Bridge](claude-code.md) |
|------|--------------|--------------------------------------|
| 消息传入方式 | stdin（原始文本） | `-p` 命令行参数；多模态走 `--input-format stream-json` 的 SDKUserMessage |
| CLI 工具 | `agent` | `claude` |
| 多模态附件 | 不支持 | 支持 image / file |

## 相关文档

- [AgentProc 0.3 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
