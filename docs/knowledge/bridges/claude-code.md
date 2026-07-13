---
type: Bridge
title: Claude Code Bridge
description: 内置 Bridge，包装 Anthropic Claude Code CLI，支持会话续传、流式输出与多模态附件（AgentProc 0.3 NDJSON）。
resource: src/bridge/builtin/claude_code.rs
tags: [bridge, claude, builtin, streaming, agentproc]
timestamp: 2026-07-13T20:30:00+08:00
---

# Claude Code Bridge

内置 Bridge，包装 **`claude` CLI**（Anthropic Claude Code），实现会话连续性、流式输出与多模态附件。采用 AgentProc 0.3 NDJSON 协议与 Hub 通信。

## 工作原理

1. 从 stdin 读取 [AgentProc 0.3 turn](/bridges/profile-protocol.md)（`message` / `session_id` / `attachments`）
2. 调用 `claude --output-format stream-json [--resume <uuid>]`
3. 消息通过 **`-p` 参数**传入（与 Cursor 不同，后者用 stdin）
4. 实时解析 stream-json 事件：每个 `assistant` 文本块 → `partial` NDJSON 事件；终端 `result.result` → `text` 事件
5. 流结束后输出 `session` 事件上报新 session id

`streaming` 是 bridge 侧 hint：agent 始终以 stream-json 运行；profile `streaming: false` 时 Bridge 不转发 `partial`，仅以 `text` 事件作为最终回复。

## 关闭流式（bridge-side hint）

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/project
    streaming: false
```

## 会话续传

- 若 turn 的 `session_id` 非空，先尝试 `--resume <uuid>`
- Resume 失败时自动回退新会话，用户透明

## 多模态附件

turn 的 `attachments` 数组支持 `image` 与 `file` 两种 kind：

- `image`：下载并 base64 编码，作为 Anthropic `image` content block（JPEG/PNG/GIF/WebP）
- `file`：作为 `document` content block（PDF 或纯文本）
- `video` 及其他 kind：发出 `error` 事件拒绝

多模态走 `--input-format stream-json --output-format stream-json`，向 stdin 写入一行 `SDKUserMessage`（`content = [text, image?, document?]`），与 Claude Code TS SDK 内部协议一致。

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
| 消息传入方式 | `-p` 参数；多模态走 `--input-format stream-json` 的 SDKUserMessage | stdin（原始文本） |
| CLI 工具 | `claude` | `agent` |
| 多模态附件 | 支持 image / file | 不支持 |

## 相关文档

- [AgentProc 0.3 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
