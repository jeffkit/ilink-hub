---
type: Bridge
title: Cursor Bridge
description: 内置 Bridge，包装 Cursor agent CLI，支持会话续传与 AgentProc 0.4 NDJSON 流式输出。
resource: https://github.com/jeffkit/im-agentproc
tags: [bridge, cursor, builtin, streaming, agentproc]
timestamp: 2026-07-16T14:30:00+08:00
---

# Cursor Bridge

> **仓库迁移（2026-07-20）**：Bridge 代码已从 `ilink-hub` 的 `src/bridge/` 物理拆分到独立仓库 [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`，bin `im-agentproc`）。本文档仍描述 Cursor Bridge 实现（概念不变），代码位置以 im-agentproc 为准。详见 `docs/proposals/bridge-as-multi-im-runtime.md` 附录 A。

内置 Bridge，包装 **Cursor `agent` CLI**，实现会话连续性与流式输出。采用 AgentProc 0.4 NDJSON 协议。

## Profile 示例

```yaml
description: Cursor agent on project
agentproc:
  executor: cursor
  cwd: /path/to/your/project
  # env:
  #   CURSOR_MODEL: …
```

## 工作原理

1. 从 stdin 读取 [AgentProc 0.4 turn](profile-protocol.md)
2. 调用 `agent --print --trust --yolo --output-format stream-json [--model …] [--resume <uuid>]`
3. 消息写入 `agent` **stdin**
4. `assistant` 文本块 → `partial`；终端 → AgentProc `result` + `session_id`

`streaming` 是 bridge 侧 hint；`streaming: false` 时不转发 `partial`。

## 会话续传

- `session_id` 非空时尝试 `--resume`
- 失败则回退新会话

## 已知差异（vs Claude Code Bridge）

| 特性 | Cursor | [Claude Code](claude-code.md) |
|------|--------|-------------------------------|
| 消息传入 | stdin | `-p` / 多模态 stream-json |
| CLI | `agent` | `claude` |
| 多模态 | 不支持 | image / file |

## 相关文档

- [AgentProc 0.4 协议与 Profile](profile-protocol.md)
- [Bridge 概览](overview.md)
