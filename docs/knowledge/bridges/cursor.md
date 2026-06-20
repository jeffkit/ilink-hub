---
type: Bridge
title: Cursor Bridge
description: 内置 Bridge，包装 Cursor agent CLI，支持会话续传和流式输出。
resource: src/bridge/builtin/cursor.rs
tags: [bridge, cursor, builtin, streaming]
timestamp: 2026-06-18T10:00:00+08:00
---

# Cursor Bridge

内置 Bridge，包装 **Cursor `agent` CLI**，实现会话连续性和流式输出。

## 工作原理

1. 读取 [P0 环境变量](/bridges/profile-protocol.md)（`ILINK_MESSAGE`、`ILINK_SESSION_ID` 等）
2. 调用 `agent --print --trust --yolo --output-format stream-json [--model <model>] [--resume <uuid>]`
3. 消息写入 `agent` 进程的 **stdin**（与 Claude Code 不同，后者用 `-p` 参数）
4. 实时解析 stream-json 事件，每个 assistant 文本块输出一行 `ILINK_PARTIAL:<json>`
5. 流结束后输出 `ILINK_SESSION:<new_session_id>`，不再输出正文（避免重复发送）

## 会话续传

- 若 `ILINK_SESSION_ID` 非空，先尝试 `--resume <uuid>`
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
| 消息传入方式 | stdin | `-p` 命令行参数 |
| CLI 工具 | `agent` | `claude` |
| `ILINK_STREAMING=0` 支持 | 否 | 是 |

## 相关文档

- [P0 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
