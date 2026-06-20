---
type: Concept
title: Bridge 概览
description: Bridge 是 ilink-hub 的客户端适配层，每种 AI 工具对应一种 Bridge 实现。
tags: [bridge, architecture]
timestamp: 2026-06-18T10:00:00+08:00
---

# Bridge 概览

Bridge 是 ilink-hub 的**客户端适配层**：Hub 通过 Bridge 与各种 AI 工具通信，将微信消息转发给对应的 AI 后端并把回复传回用户。

## 内置 Bridge

| Bridge | 文件 | 适配目标 |
|--------|------|---------|
| `cursor` | `src/bridge/builtin/cursor.rs` | [Cursor Bridge](cursor.md) |
| `claude-code` | `src/bridge/builtin/claude_code.rs` | [Claude Code Bridge](claude-code.md) |

## Bridge 的工作方式

1. AI 工具向 Hub 注册（WebSocket 连接 + 认证）
2. 用户在微信发消息 → Hub 路由到当前活跃 Bridge
3. Bridge 将消息转换为目标工具的格式并发送
4. Bridge 读取工具响应，实时流式转发回微信

## Profile 机制

每个 Bridge 的运行参数由 **Profile** 配置。Profile 定义了要启动的进程、工作目录、环境变量等。详见 [P0 协议与 Profile](profile-protocol.md)。

## Bridge 模块结构

```
src/bridge/
├── mod.rs              # Bridge trait 定义与注册
└── builtin/
    ├── cursor.rs       # Cursor Bridge 实现
    └── claude_code.rs  # Claude Code Bridge 实现
```

## 相关文档

- [P0 协议与 Profile](profile-protocol.md) — 进程启动约定
- [微信命令](/api/commands.md) — `/use <name>` 切换 Bridge
